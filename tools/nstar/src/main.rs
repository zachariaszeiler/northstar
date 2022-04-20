//! Northstar console client

#![deny(clippy::all)]
#![deny(missing_docs)]

use anyhow::{anyhow, bail, Context, Result};
use api::{client::Client, model::Message};
use clap::{self, IntoApp, Parser};
use futures::{sink::SinkExt, StreamExt};
use nix::{sys::select, unistd};
use northstar::{
    api::{
        self,
        model::{Container, NonNulString, Request},
    },
    common::{name::Name, version::Version},
};
use std::{
    collections::HashMap,
    convert::{TryFrom, TryInto},
    os::unix::prelude::{AsRawFd, RawFd},
    path::PathBuf,
    process,
};
use tokio::{
    fs,
    io::{copy, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    net::{TcpStream, UnixStream},
    time,
};

trait N: AsyncRead + AsyncWrite + Send + Unpin + AsRawFd {}
impl<T> N for T where T: AsyncRead + AsyncWrite + Send + Unpin + AsRawFd {}

impl AsRawFd for Box<dyn N> {
    fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        (**self).as_raw_fd()
    }
}

mod pretty;

/// Default nstar address
const DEFAULT_HOST: &str = "tcp://localhost:4200";

/// About string for CLI
fn about() -> &'static str {
    Box::leak(Box::new(format!(
        "Northstar API version {}",
        api::model::version()
    )))
}

/// Subcommands
#[derive(Parser, Clone)]
#[clap(name = "nstar", author, about = about())]
enum Subcommand {
    /// List available containers
    #[clap(alias = "ls", alias = "list")]
    Containers,
    /// List configured repositories
    #[clap(alias = "repos")]
    Repositories,
    /// Mount a container
    Mount {
        /// Container name and optional version
        #[clap(value_name = "name[:version]")]
        containers: Vec<String>,
    },
    /// Umount a container
    Umount {
        /// Container name and optional version
        #[clap(value_name = "name[:version]")]
        containers: Vec<String>,
    },
    /// Start a container
    Start {
        /// Container name and optional version
        #[clap(value_name = "name[:version]")]
        container: String,
        /// Command line arguments
        #[clap(short, long)]
        args: Option<Vec<String>>,
        /// Environment variables in KEY=VALUE format
        #[clap(short, long)]
        env: Option<Vec<String>>,
    },
    /// Stop a container
    Kill {
        /// Container name and optional version
        #[clap(value_name = "name[:version]")]
        container: String,
        /// Signal
        signal: Option<i32>,
    },
    /// Start an additional process inside a container
    Exec {
        /// Container name and optional version
        #[clap(value_name = "name[:version]")]
        container: String,
        /// Command
        path: String,
        /// Command arguments
        args: Vec<String>,
        /// Environment variables
        #[structopt(short, long)]
        env: Vec<String>,
    },
    /// Install a npk
    Install {
        /// Path to the .npk file
        npk: PathBuf,
        /// Target repository
        repository: String,
    },
    /// Uninstall a container
    Uninstall {
        /// Container name and optional version
        #[clap(value_name = "name[:version]")]
        container: String,
    },
    /// Shutdown Northstar
    Shutdown,
    /// Notifications
    Notifications {
        /// Exit after n notifications
        #[clap(short, long)]
        number: Option<usize>,
    },
    /// Shell completion script generation
    Completion {
        /// Output directory where to generate completions into
        #[clap(short, long)]
        output: Option<PathBuf>,
        /// Generate completions for shell type
        #[clap(short, long)]
        shell: clap_complete::Shell,
    },
    /// Request container statistics
    ContainerStats {
        /// Container name and optional version
        #[clap(value_name = "name[:version]")]
        container: String,
    },
}

/// CLI
#[derive(Parser)]
struct Opt {
    /// Northstar address
    #[clap(short, long, default_value = DEFAULT_HOST)]
    pub url: url::Url,
    /// Output json
    #[clap(short, long)]
    pub json: bool,
    /// Connect timeout in seconds
    #[clap(short, long, default_value = "10", parse(try_from_str = parse_secs))]
    pub timeout: time::Duration,
    /// Command
    #[clap(subcommand)]
    pub command: Subcommand,
}

/// Parse a str containing a u64 into a `std::time::Duration` and take the value
/// as seconds
fn parse_secs(src: &str) -> Result<time::Duration, anyhow::Error> {
    src.parse::<u64>()
        .map(time::Duration::from_secs)
        .map_err(Into::into)
}

/// Parse the container name and version out of the user input
///
/// # Format
///
/// The string format for the container name is specified as `<name>[:<version>]`.
///
/// if the version is not specified, Northstar is queried for all the versions associated to
/// `<name>` and only if a single version is found, it is used.
///
async fn parse_container<T>(name: &str, client: &mut Client<T>) -> Result<Container>
where
    T: AsyncRead + AsyncWrite + Unpin + AsRawFd,
{
    let (name, version): (Name, Version) = if let Some((name, version)) = name.split_once(':') {
        (Name::try_from(name)?, Version::parse(version)?)
    } else {
        let name = Name::try_from(name)?;
        let versions: Vec<Version> = client
            .containers()
            .await?
            .into_iter()
            .filter_map(|c| (c.manifest.name == name).then(|| c.manifest.version))
            .collect();

        if versions.is_empty() {
            bail!("no container found with name {}", name);
        } else if versions.len() > 1 {
            bail!("container {} has multiple versions: {:?}", name, versions);
        } else {
            (name, versions[0].clone())
        }
    };
    Ok(Container::new(name, version))
}

async fn command_to_request<T: AsyncRead + AsyncWrite + Unpin + AsRawFd>(
    command: Subcommand,
    client: &mut Client<T>,
) -> Result<Request> {
    match command {
        Subcommand::Containers => Ok(Request::Containers),
        Subcommand::Repositories => Ok(Request::Repositories),
        Subcommand::Mount { containers } => {
            let mut converted = Vec::with_capacity(containers.len());
            for container in containers {
                converted.push(parse_container(&container, client).await?);
            }
            Ok(Request::Mount {
                containers: converted,
            })
        }
        Subcommand::Umount { containers } => {
            let mut converted = Vec::with_capacity(containers.len());
            for container in containers {
                converted.push(parse_container(&container, client).await?);
            }
            Ok(Request::Umount {
                containers: converted,
            })
        }
        Subcommand::Start {
            container,
            args,
            env,
        } => {
            let container = parse_container(&container, client).await?;

            // Convert args
            let args = if let Some(args) = args {
                let mut non_null = Vec::with_capacity(args.len());
                for arg in args {
                    non_null.push(NonNulString::try_from(arg.as_str()).context("invalid arg")?);
                }
                non_null
            } else {
                Vec::with_capacity(0)
            };

            // Convert env
            let env = if let Some(env) = env {
                let mut non_null = HashMap::with_capacity(env.len());
                for env in env {
                    let mut split = env.split('=');
                    let key = split
                        .next()
                        .ok_or_else(|| anyhow!("invalid env"))
                        .and_then(|s| NonNulString::try_from(s).context("invalid key"))?;
                    let value = split
                        .next()
                        .ok_or_else(|| anyhow!("invalid env"))
                        .and_then(|s| NonNulString::try_from(s).context("invalid value"))?;
                    non_null.insert(key, value);
                }
                non_null
            } else {
                HashMap::with_capacity(0)
            };

            Ok(Request::Start {
                container,
                args,
                env,
            })
        }
        Subcommand::Kill { container, signal } => {
            let container = parse_container(&container, client).await?;
            let signal = signal.unwrap_or(15);
            Ok(Request::Kill { container, signal })
        }
        Subcommand::Install { npk, repository } => {
            let size = npk.metadata().map(|m| m.len())?;
            Ok(Request::Install { repository, size })
        }
        Subcommand::Uninstall { container } => Ok(Request::Uninstall {
            container: parse_container(&container, client).await?,
        }),
        Subcommand::Shutdown => Ok(Request::Shutdown),
        Subcommand::ContainerStats { container } => {
            let container = parse_container(&container, client).await?;
            Ok(Request::ContainerStats { container })
        }
        Subcommand::Notifications { .. } | Subcommand::Completion { .. } => unreachable!(),
        // This subcommand is handled separately
        Subcommand::Exec { .. } => unreachable!(),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let opt = Opt::parse();
    let timeout = time::Duration::from_secs(5);

    // Generate shell completions and exit on give subcommand
    if let Subcommand::Completion { output, shell } = opt.command {
        let mut output: Box<dyn std::io::Write> = match output {
            Some(path) => {
                println!("Generating {} completions to {}", shell, path.display());
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .open(&path)
                    .with_context(|| format!("failed to open {}", path.display()))?;
                Box::new(file)
            }
            None => Box::new(std::io::stdout()),
        };

        clap_complete::generate(
            shell,
            &mut Opt::command(),
            Opt::command().get_name().to_string(),
            &mut output,
        );

        process::exit(0);
    }

    let io = match opt.url.scheme() {
        "tcp" => {
            let addresses = opt.url.socket_addrs(|| Some(4200))?;
            let address = addresses
                .first()
                .ok_or_else(|| anyhow!("failed to resolve {}", opt.url))?;
            let stream = time::timeout(timeout, TcpStream::connect(address))
                .await
                .context("failed to connect")??;

            Box::new(stream) as Box<dyn N>
        }
        "unix" => {
            let stream = time::timeout(timeout, UnixStream::connect(opt.url.path()))
                .await
                .context("failed to connect")??;
            Box::new(stream) as Box<dyn N>
        }
        _ => return Err(anyhow!("invalid url")),
    };

    match opt.command {
        // Subscribe to notifications and print them
        Subcommand::Notifications { number } => {
            if opt.json {
                let mut framed = Client::new(io, Some(100), opt.timeout)
                    .await
                    .with_context(|| format!("failed to connect to {}", opt.url))?
                    .framed();

                let mut lines = BufReader::new(framed.get_mut()).lines();
                for _ in 0..number.unwrap_or(usize::MAX) {
                    match lines.next_line().await.context("failed to read stream")? {
                        Some(line) => println!("{}", line),
                        None => break,
                    }
                }
            } else {
                let client = Client::new(io, Some(100), opt.timeout)
                    .await
                    .with_context(|| format!("failed to connect to {}", opt.url))?;
                let mut notifications = client.take(number.unwrap_or(usize::MAX));
                while let Some(notification) = notifications.next().await {
                    let notification = notification.context("failed to receive notification")?;
                    pretty::notification(&notification);
                }
                process::exit(0);
            }
        }
        // Start process inside running container and attach a pseudo-terminal to it
        Subcommand::Exec {
            container,
            env,
            path,
            args,
        } => {
            // Connect
            let mut client = Client::new(io, None, opt.timeout)
                .await
                .context("Failed to connect")?;

            let container = parse_container(&container, &mut client).await?;

            let env: Result<Vec<_>> = env
                .into_iter()
                .map(|c| c.try_into().context("Invalid arg"))
                .collect();

            let path = path.try_into().context("Invalid arg")?;

            let args: Result<Vec<_>> = args
                .iter()
                .map(|c| c.as_str().try_into().context("Invalid arg"))
                .collect();

            let request = Request::Exec {
                container,
                env: env?,
                path,
                args: args?,
                pty: None,
            };

            client
                .request(request)
                .await
                .context("Failed to send request")?;

            let connection = client.into_inner();
            let master = connection.as_raw_fd();
            bridge(master, nix::libc::STDIN_FILENO)?;
        }
        // Request response mode
        command => {
            // Connect
            let mut client = Client::new(io, None, opt.timeout)
                .await
                .context("failed to connect")?;

            // Convert the subcommand into a request
            let request = command_to_request(command.clone(), &mut client)
                .await
                .context("failed to convert command into request")?;

            // If the raw json mode is requested nstar needs to operate on the raw stream instead
            // of `Client<T>`
            let mut framed = client.framed();

            framed
                .send(Message::Request { request })
                .await
                .context("failed to send request")?;

            // Extra file transfer for install hack
            if let Subcommand::Install { npk, .. } = command {
                framed.flush().await.context("failed to flush")?;
                framed.get_mut().flush().await.context("failed to flush")?;

                copy(
                    &mut fs::File::open(npk).await.context("failed to open npk")?,
                    &mut framed.get_mut(),
                )
                .await
                .context("failed to stream npk")?;
            }

            framed.get_mut().flush().await.context("failed to flush")?;

            if opt.json {
                let response = BufReader::new(framed.get_mut())
                    .lines()
                    .next_line()
                    .await
                    .context("failed to receive response")?
                    .ok_or_else(|| anyhow!("failed to receive response"))?;
                println!("{}", response);
                process::exit(0);
            } else {
                // Read next deserialized response and pretty print
                let exit = match framed
                    .next()
                    .await
                    .ok_or_else(|| anyhow!("failed to receive response"))??
                {
                    api::model::Message::Response { response } => pretty::response(&response),
                    _ => unreachable!(),
                };
                process::exit(exit);
            }
        }
    };

    Ok(())
}

// TODO refactor using tokio
/// Sends io back an forth between the input FDs
fn bridge<L, R>(lhs: L, rhs: R) -> std::io::Result<()>
where
    L: AsRawFd,
    R: AsRawFd,
{
    let lhs = lhs.as_raw_fd();
    let rhs = rhs.as_raw_fd();

    loop {
        let mut fd_set = select::FdSet::new();
        fd_set.insert(lhs);
        fd_set.insert(rhs);

        select::select(None, &mut fd_set, None, None, None).map_err(io_err)?;

        if fd_set.contains(lhs) && copy_bytes(lhs, rhs)? == 0 {
            return Ok(());
        }

        if fd_set.contains(rhs) && copy_bytes(rhs, lhs)? == 0 {
            return Ok(());
        }
    }
}

/// Copy bytes from one FD to another.
fn copy_bytes(from: RawFd, to: RawFd) -> std::io::Result<usize> {
    let mut buf = [0; 512];
    let n = unistd::read(from, &mut buf).map_err(io_err)?;

    if n > 0 {
        unistd::write(to, &buf[..n]).map_err(io_err)
    } else {
        Ok(0)
    }
}

fn io_err(err: nix::Error) -> std::io::Error {
    std::io::Error::from_raw_os_error(err as i32)
}
