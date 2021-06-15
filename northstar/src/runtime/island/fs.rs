// Copyright (c) 2021 ESRLabs
//
//   Licensed under the Apache License, Version 2.0 (the "License");
//   you may not use this file except in compliance with the License.
//   You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
//   Unless required by applicable law or agreed to in writing, software
//   distributed under the License is distributed on an "AS IS" BASIS,
//   WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
//   See the License for the specific language governing permissions and
//   limitations under the License.

use super::{Container, Error};
use crate::runtime::{config::Config, island::utils::PathExt};
use log::debug;
use nix::{
    libc::makedev,
    mount::MsFlags,
    sys::stat::{Mode, SFlag},
    unistd,
    unistd::{chown, Gid, Uid},
};
use npk::manifest::{self, MountOption, MountOptions, Resource, Tmpfs};
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use tokio::{fs::symlink, task};

/// The minimal version of the /dev is maintained in a tmpdir. This tmpdir
/// must be held for the lifetime of the IslandProcess
pub(crate) type Dev = Option<TempDir>;

/// Mount systemcall instruction done in init
#[derive(Debug)]
pub(super) struct Mount {
    pub source: Option<PathBuf>,
    pub target: PathBuf,
    pub fstype: Option<&'static str>,
    pub flags: MsFlags,
    pub data: Option<String>,
    pub err_str: String,
}

impl Mount {
    /// Execute this mount call
    pub(super) fn mount(&self) -> Result<(), ()> {
        nix::mount::mount(
            self.source.as_ref(),
            &self.target,
            self.fstype,
            self.flags,
            self.data.as_deref(),
        )
        .expect(&self.err_str);
        Ok(())
    }
}

/// Iterate the mounts of a container and assemble a list of `mount` calls to be
/// performed by init. Prepare an options persist dir. This fn fails if a resource
/// is referenced that does not exist.
pub(super) async fn prepare_mounts(
    config: &Config,
    container: &Container,
) -> Result<(Vec<Mount>, Dev), Error> {
    let mut mounts = Vec::new();
    let mut dev = None;
    let root = container
        .root
        .canonicalize()
        .map_err(|e| Error::io("Canonicalize root", e))?;

    mounts.push(proc(&root));

    let manifest_mounts = &container.manifest.mounts;

    for (target, mount) in manifest_mounts {
        match &mount {
            manifest::Mount::Bind(manifest::Bind { host, options }) => {
                mounts.extend(bind(&root, target, host, options));
            }
            manifest::Mount::Persist => {
                mounts.push(persist(&root, target, config, container).await?);
            }
            manifest::Mount::Resource(res) => {
                let (mount, remount_ro) = resource(&root, target, config, container, res)?;
                mounts.push(mount);
                mounts.push(remount_ro);
            }
            manifest::Mount::Tmpfs(Tmpfs { size }) => mounts.push(tmpfs(&root, target, *size)),
            manifest::Mount::Dev => {
                let (d, mount, remount) = self::dev(&root, &container).await;
                mounts.push(mount);
                mounts.push(remount);
                dev = d
            }
        }
    }

    // No dev configured in mounts: Use minimal version
    if dev.is_none() && !manifest_mounts.contains_key(Path::new("/dev")) {
        let (d, mount, remount) = self::dev(&root, &container).await;
        mounts.push(mount);
        mounts.push(remount);
        dev = d;
    }

    Ok((mounts, dev))
}

fn proc(root: &Path) -> Mount {
    debug!("Mounting /proc");
    let target = root.join("proc");
    let err_str = format!("Failed to mount {}", &target.display());
    let flags = MsFlags::MS_RDONLY | MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC | MsFlags::MS_NODEV;
    Mount {
        source: Some(PathBuf::from("proc")),
        target,
        fstype: Some("proc"),
        flags,
        data: None,
        err_str,
    }
}

fn bind(root: &Path, target: &Path, host: &Path, options: &MountOptions) -> Vec<Mount> {
    if host.exists() {
        let rw = options.contains(&MountOption::Rw);
        let mut mounts = Vec::with_capacity(if rw { 2 } else { 1 });
        debug!(
            "Mounting {} on {} with {:?}",
            host.display(),
            target.display(),
            options.iter().collect::<Vec<_>>(),
        );
        let target = root.join_strip(target);
        let err_str = format!("Failed to mount {}", &target.display());
        let mut flags = options_to_flags(&options);
        flags.set(MsFlags::MS_BIND, true);
        mounts.push(Mount {
            source: Some(host.to_owned()),
            target: target.clone(),
            fstype: None,
            flags: MsFlags::MS_BIND | flags,
            data: None,
            err_str: err_str.clone(),
        });

        if !rw {
            mounts.push(Mount {
                source: Some(host.to_owned()),
                target,
                fstype: None,
                flags: MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY | flags,
                data: None,
                err_str,
            });
        }
        mounts
    } else {
        debug!(
            "Skipping bind mount of nonexitent source {} to {}",
            host.display(),
            target.display()
        );
        vec![]
    }
}

async fn persist(
    root: &Path,
    target: &Path,
    config: &Config,
    container: &Container,
) -> Result<Mount, Error> {
    let uid = container.manifest.uid;
    let gid = container.manifest.gid;
    let dir = config.data_dir.join(&container.manifest.name);

    if !dir.exists() {
        debug!("Creating {}", dir.display());
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| Error::Io(format!("Failed to create {}", dir.display()), e))?;
    }

    debug!("Chowning {} to {}:{}", dir.display(), uid, gid);
    task::block_in_place(|| {
        unistd::chown(
            dir.as_os_str(),
            Some(unistd::Uid::from_raw(uid)),
            Some(unistd::Gid::from_raw(gid)),
        )
    })
    .map_err(|e| {
        Error::os(
            format!("Failed to chown {} to {}:{}", dir.display(), uid, gid),
            e,
        )
    })?;

    debug!("Mounting {} on {}", dir.display(), target.display(),);

    let target = root.join_strip(target);
    let err_str = format!("Failed to mount {}", &target.display());
    let flags = MsFlags::MS_BIND | MsFlags::MS_NODEV | MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC;
    Ok(Mount {
        source: Some(dir),
        target,
        fstype: None,
        flags,
        data: None,
        err_str,
    })
}

fn resource(
    root: &Path,
    target: &Path,
    config: &Config,
    container: &Container,
    resource: &Resource,
) -> Result<(Mount, Mount), Error> {
    let src = {
        // Join the source of the resource container with the mount dir
        let resource_root = config
            .run_dir
            .join(format!("{}:{}", resource.name, resource.version));
        let dir = resource
            .dir
            .strip_prefix("/")
            .map(|d| resource_root.join(d))
            .unwrap_or(resource_root);

        if !dir.exists() {
            return Err(Error::StartContainerMissingResource(
                container.container.clone(),
                container.container.clone(),
            ));
        }

        dir
    };

    debug!(
        "Mounting {} on {} with {:?}",
        src.display(),
        target.display(),
        resource.options
    );

    let mut flags = options_to_flags(&resource.options);
    flags |= MsFlags::MS_RDONLY | MsFlags::MS_BIND;

    let target = root.join_strip(target);
    let err_str = format!("Failed to mount {}", &target.display());
    let mount = Mount {
        source: Some(src.clone()),
        target: target.clone(),
        fstype: None,
        flags,
        data: None,
        err_str: err_str.clone(),
    };

    // Remount ro
    let remount_ro = Mount {
        source: Some(src),
        target,
        fstype: None,
        flags: MsFlags::MS_REMOUNT | flags,
        data: None,
        err_str,
    };
    Ok((mount, remount_ro))
}

fn tmpfs(root: &Path, target: &Path, size: u64) -> Mount {
    debug!(
        "Mounting tmpfs with size {} on {}",
        bytesize::ByteSize::b(size),
        target.display()
    );
    let target = root.join_strip(target);
    let err_str = format!("Failed to mount {}", &target.display());
    Mount {
        source: None,
        target,
        fstype: Some("tmpfs"),
        flags: MsFlags::MS_NODEV | MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
        data: Some(format!("size={},mode=1777", size)),
        err_str,
    }
}

fn options_to_flags(opt: &MountOptions) -> MsFlags {
    let mut flags = MsFlags::empty();
    for opt in opt {
        match opt {
            MountOption::Rw => {}
            MountOption::NoExec => flags |= MsFlags::MS_NOEXEC,
            MountOption::NoSuid => flags |= MsFlags::MS_NOSUID,
            MountOption::NoDev => flags |= MsFlags::MS_NODEV,
        }
    }
    flags
}

async fn dev(root: &Path, container: &Container) -> (Dev, Mount, Mount) {
    let dir = task::block_in_place(|| TempDir::new().expect("Failed to create tempdir"));
    debug!("Creating devfs in {}", dir.path().display());

    task::block_in_place(|| {
        dev_devices(dir.path(), container.manifest.uid, container.manifest.gid)
    });
    dev_symlinks(dir.path()).await;

    let flags = MsFlags::MS_BIND | MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC;
    let err_str = format!("Failed to mount {}", &root.join("dev").display());

    let mount = Mount {
        source: Some(dir.path().into()),
        target: root.join("dev"),
        fstype: None,
        flags,
        data: None,
        err_str: err_str.clone(),
    };

    let remount = Mount {
        source: Some(dir.path().into()),
        target: root.join("dev"),
        fstype: None,
        flags: MsFlags::MS_REMOUNT | flags,
        data: None,
        err_str,
    };
    (Some(dir), mount, remount)
}

fn dev_devices(dir: &Path, uid: u32, gid: u32) {
    use nix::sys::stat::mknod;

    for (dev, major, minor) in &[
        ("full", 1, 7),
        ("null", 1, 3),
        ("random", 1, 8),
        ("tty", 5, 0),
        ("urandom", 1, 9),
        ("zero", 1, 5),
    ] {
        let dev_path = dir.join(dev);
        let dev = unsafe { makedev(*major, *minor) };
        mknod(dev_path.as_path(), SFlag::S_IFCHR, Mode::all(), dev).expect("Failed to mknod");
        chown(
            dev_path.as_path(),
            Some(Uid::from_raw(uid)),
            Some(Gid::from_raw(gid)),
        )
        .expect("Failed to chown");
    }
}

async fn dev_symlinks(dir: &Path) {
    let kcore = Path::new("/proc/kcore");
    if kcore.exists() {
        symlink(kcore, dir.join("kcore"))
            .await
            .expect("Failed to create symlink");
    }

    let defaults = [
        ("/proc/self/fd", "fd"),
        ("/proc/self/fd/0", "stdin"),
        ("/proc/self/fd/1", "stdout"),
        ("/proc/self/fd/2", "stderr"),
    ];
    for &(src, dst) in defaults.iter() {
        symlink(src, dir.join(dst))
            .await
            .expect("Failed to create symlink");
    }
}
