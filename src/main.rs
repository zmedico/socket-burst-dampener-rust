use async_std::sync::{channel, Sender, Receiver};
use tokio::spawn;
use tokio::net::TcpListener;

static LOGGER: BasicLogger = BasicLogger;

struct BasicLogger;

impl log::Log for BasicLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            println!("[{}] - {}", record.level(), record.args());
        }
    }
    fn flush(&self) {}
}

#[derive(Debug)]
enum ErrorType {
    IOError(std::io::Error),
    Message(String),
}

impl std::convert::From<std::io::Error> for ErrorType {
    fn from(e: std::io::Error) -> Self {
        ErrorType::IOError(e)
    }
}

impl std::convert::From<std::string::String> for ErrorType {
    fn from(e: std::string::String) -> Self {
        ErrorType::Message(e)
    }
}

struct Config{
    addresses: Vec<std::net::SocketAddr>,
    cmd: String,
    args: Vec<String>,
    processes: u32,
    load_average: f32,
}

enum Event {
    AcceptedConnection(i32),
    CoroutinePanic(Result<(), ErrorType>),
    ProcessExit(u32),
}

async fn waitpid_loop(sender: &Sender<Event>) -> Result<(), ErrorType> {
    let mut status = 0;
    let mut stream = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())?;

    while stream.recv().await.is_some() {
        loop {
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if pid <= 0 {
                break;
            }
            use std::convert::TryInto;
            sender.send(Event::ProcessExit(pid.try_into().unwrap())).await;
        }
    }
    Ok(())
}

async fn listen_loop(addresses: Vec<std::net::SocketAddr>, accept_receiver: &Receiver<()>, event_sender: &Sender<Event>) -> Result<(), ErrorType> {

    /* The size of this channel limits the number of accepted connections
     * lacking a response at any given time, and we don't want to have
     * very many in this channel at the same time since we'd prefer to
     * have the kernel manage them in its connection queue up until we're
     * prepared to respond.
     */
    let (stream_sender, stream_receiver) = channel::<i32>(1);
    for addr in addresses.iter() {
        {
            let stream_sender = stream_sender.clone();
            let addr = *addr;
            let exit_sender = event_sender.clone();
            spawn(async move {
                let mut listener = match TcpListener::bind(addr).await {
                    Ok(listener) => listener,
                    Err(e) => {
                        exit_sender.send(Event::CoroutinePanic(Err(ErrorType::from(e)))).await;
                        return
                    },
                };

                loop {
                    let (socket, _addr) = match listener.accept().await {
                        Ok((socket, _addr)) => (socket, _addr),
                        Err(_) => continue,
                    };

                    use async_std::os::unix::io::AsRawFd;
                    let stream_fd = nix::unistd::dup(socket.as_raw_fd()).unwrap();

                    /* We have a default O_NONBLOCK setting from the underlying
                     * mio library, and that would be a bad default for stdio of
                     * a child process.
                     */
                    let mut flags = nix::fcntl::OFlag::from_bits(nix::fcntl::fcntl(stream_fd, nix::fcntl::FcntlArg::F_GETFL).unwrap()).unwrap();
                    flags.remove(nix::fcntl::OFlag::O_NONBLOCK);
                    // The O_CLOEXEC flag is eliminated at the last moment.
                    flags.insert(nix::fcntl::OFlag::O_CLOEXEC);
                    nix::fcntl::fcntl(stream_fd, nix::fcntl::FcntlArg::F_SETFL(flags)).unwrap();

                    stream_sender.send(stream_fd).await;
                }
            });
        }
    }

    loop {
        /* Wait here in order to provide backpressure so that we don't
         * accept too many connections before the main loop is prepared
         * for them (depends on system load).
         */
        accept_receiver.recv().await;

        let stream = stream_receiver.recv().await.unwrap();
        log::debug!("accept");
        event_sender.send(Event::AcceptedConnection(stream)).await;
    }
}

async fn main_loop(config: Config) -> Result<(), ErrorType> {

    // Set this size at least to the number of event types that occur frequently (AcceptedConnection and ProcessExit).
    let (event_sender, event_receiver) = channel::<Event>(2);
    let (accept_sender, accept_receiver) = channel::<()>(1);

    let listen_addresses = config.addresses.clone();
    let listen_event_sender = event_sender.clone();
    spawn(async move {
        let result = listen_loop(listen_addresses, &accept_receiver, &listen_event_sender).await;
        listen_event_sender.send(Event::CoroutinePanic(result)).await;
    });

    let waitpid_event_sender = event_sender.clone();
    spawn(async move {
        let result = waitpid_loop(&waitpid_event_sender).await;
        waitpid_event_sender.send(Event::CoroutinePanic(result)).await;
    });

	let acceptable_load = |sessions: &std::collections::HashSet::<u32>| -> bool {
		config.load_average == 0.0 ||
        sessions.len() < 1 ||
        sessions.len() < config.processes as usize && nix::sys::sysinfo::sysinfo().unwrap().load_average().0 < config.load_average.into()
	};

    let mut sessions = std::collections::HashSet::<u32>::new();
    let mut accepting = true;

    // Accept the first connection.
    accept_sender.send(()).await;

    loop {
        match event_receiver.recv().await.unwrap() {
            Event::AcceptedConnection(stream_fd) => {
                let stream_stdin: std::process::Stdio;
                let stream_stdout: std::process::Stdio;
                unsafe {
                    use std::os::unix::io::FromRawFd;
                    stream_stdin = std::process::Stdio::from_raw_fd(stream_fd);
                    stream_stdout = std::process::Stdio::from_raw_fd(stream_fd);
                }
                let child = std::process::Command::new(config.cmd.clone())
                    .args(config.args.clone())
                    .stdin(stream_stdin)
                    .stdout(stream_stdout)
                    .stderr(std::process::Stdio::inherit())
                    .spawn()
                    .unwrap();
                // Command::spawn has now closed stream_fd

                sessions.insert(child.id());
                log::debug!("session start");

                if !acceptable_load(&sessions) {
                    accepting = false;
                } else {
                    accept_sender.send(()).await;
                }
            }
            Event::ProcessExit(pid) => {
                sessions.remove(&pid);
                log::debug!("session end");
                if sessions.len() == 0 {
                    log::debug!("no remaining sessions");
                }
                if !accepting && acceptable_load(&sessions) {
                    accepting = true;
                    accept_sender.send(()).await;
                }
            }
            Event::CoroutinePanic(result) => {
                return result;
            }
        }
    }
}

fn parse_args() -> clap::ArgMatches<'static> {

    use clap::{App, Arg};

    App::new("socket-burst-dampener")
        .about("A daemon that spawns a specified command to handle each connection, and dampens connection bursts")
        .arg(Arg::with_name("address")
           .long("address")
           .value_name("ADDRESS")
           .help("Bind to the specified address")
           .takes_value(true))
        .arg(Arg::with_name("load-average")
           .long("load-average")
           .value_name("LOAD")
           .help("Don't accept multiple connections unless load is below")
           .takes_value(true)
           .default_value("0.0"))
        .arg(Arg::with_name("processes")
           .long("processes")
           .value_name("PROCESSES")
           .help("Maximum number of concurrent processes")
           .takes_value(true)
           .default_value("1"))
        .arg(Arg::with_name("PORT")
           .help("Listen on the given port number")
           .required(true)
           .index(1))
        .arg(Arg::with_name("CMD")
           .help("Command to spawn to handle each connection")
           .required(true)
           .index(2))
        .arg(Arg::with_name("ARGS")
           .value_name("ARG")
           .help("Argument(s) for CMD")
           .required(false)
           .multiple(true))
        .arg(Arg::with_name("v")
           .short("v")
           .multiple(true)
           .help("Sets the level of verbosity"))
        .arg(Arg::with_name("ipv4")
           .long("ipv4")
           .help("Prefer IPv4"))
        .arg(Arg::with_name("ipv6")
           .long("ipv6")
           .help("Prefer IPv6"))
        .arg(Arg::with_name("version")
           .short("V")
           .long("version")
           .hidden(true))
        .get_matches()
}

fn ipv6_bindv6only() -> bool {
    // On Linux, when bindv6only contains 0, :: also binds 0.0.0.0 (dual stack support).
    let contents = match std::fs::read_to_string("/proc/sys/net/ipv6/bindv6only") {
        Ok(contents) => contents,
        Err(_) => return true,
    };

    ! contents.find("0").is_some()
}

// threaded_scheduler is not needed
#[tokio::main(basic_scheduler)]
async fn main() -> Result<(), ErrorType> {

    use clap::value_t;
    let matches = parse_args();
    let verbosity = matches.occurrences_of("v");

    log::set_logger(&LOGGER).unwrap();
    if verbosity > 0 {
        if verbosity >= 2 {
            log::set_max_level(log::LevelFilter::Debug);
        } else {
            log::set_max_level(log::LevelFilter::Info);
        }
    } else {
        log::set_max_level(log::LevelFilter::Warn);
    }

    let port = match value_t!(matches.value_of("PORT"), u32) {
        Ok(port) => port,
        Err(_) => return Err(ErrorType::from(
            format!("PORT: not a valid integer: {}", matches.value_of("PORT").unwrap()))),
    };

    let cmd = matches.value_of("CMD").unwrap().to_string();
    let args: Vec<String>;
    if matches.is_present("ARGS") {
        args = matches.values_of("ARGS").unwrap().map(|arg| arg.to_string()).collect();
    } else {
        args = Vec::new();
    }

    let processes = match value_t!(matches.value_of("processes"), u32) {
        Ok(processes) => processes,
        Err(_) => return Err(ErrorType::from(
            format!("--processes: not a valid integer: {}", matches.value_of("processes").unwrap()))),
    };

    let load_average = match value_t!(matches.value_of("load-average"), f32) {
        Ok(load_average) => load_average,
        Err(_) => return Err(ErrorType::from(
            format!("--load-average: not a valid float: {}", matches.value_of("load-average").unwrap()))),
    };

    let ipv4 = matches.is_present("ipv4");
    let ipv6 = matches.is_present("ipv6");

    let mut address: Vec<String> = Vec::new();
    if matches.is_present("address") {
        address.push(matches.value_of("address").unwrap().to_string());
    } else {
        if ipv4 && ipv6 {
            if ipv6_bindv6only() {
                address.push("0.0.0.0".to_string());
            }
            address.push("::".to_string());
        } else if ipv4 {
            address.push("0.0.0.0".to_string());
        } else if ipv6 {
            address.push("::".to_string());
        } else {
            if ipv6_bindv6only() {
                address.push("0.0.0.0".to_string());
            }
            address.push("::".to_string());
        }
    }

    use std::net::{SocketAddr, ToSocketAddrs};
    let mut addresses: Vec<SocketAddr> = Vec::new();
    for addr in &address {
        let address_iter = match format!("{}:{}", addr, port).to_socket_addrs() {
            Ok(addresses) => addresses,
            Err(error) => return Err(ErrorType::from(
                format!("--address/--port: '{}:{}': {}", addr, port, error))),
        };
        addresses.extend(address_iter);
    }
    log::debug!("addresses: {:?}", addresses);

    if addresses.len() == 0 {
        return Err(ErrorType::from(
            format!("no socket addresses resolved for {}:{}", matches.value_of("address").unwrap(), port)))
    }

    let config = Config { cmd, args, addresses, processes, load_average };

    match main_loop(config).await {
        Ok(_) => return Ok(()),
        Err(error) => return Err(error),
    }
}
