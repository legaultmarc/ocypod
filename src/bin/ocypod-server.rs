//! Main executable that runs the HTTP server for the queue application.

use actix_web::{server, App, http, middleware::Logger};
use actix::prelude::{Actor, SyncArbiter};
use log::{debug, info, warn};
use num_cpus;

use ocypod::{config, models::ApplicationState};
use ocypod::actors::{application::ApplicationActor, monitor::MonitorActor};
use ocypod::handlers;

fn main() {
    let config = parse_config_from_cli_args(&parse_cli_args());

    // TODO: is env_logger the best choice here, or would slog be preferable?
    {
        let log_settings = format!("ocypod={},ocypod-server={}", config.server.log_level, config.server.log_level);
        env_logger::Builder::new()
            .parse(&log_settings)
            .default_format_module_path(false)
            .init();
        debug!("Log initialised using: {}", &log_settings);
    }

    let http_server_addr = config.server_addr();
    let redis_url = config.redis_url();
    let redis_client = match redis::Client::open(redis_url) {
        Ok(client) => client,
        Err(err) => {
            eprintln!("Failed to initialise Redis client: {}", err);
            std::process::exit(1);
        }
    };

    // initialise Actix actor system
    let sys = actix::System::new("ocypod");

    let num_workers = config.redis.threads.unwrap_or(num_cpus::get());

    // start N sync workers
    debug!("Starting {} Redis worker(s)", num_workers);
    let redis_addr = SyncArbiter::start(num_workers, move || {
        ApplicationActor::new(redis_client.clone())
    });
    info!("{} Redis worker(s) started", num_workers);

    // start actor that executes periodic tasks
    let _monitor_addr = MonitorActor::new(redis_addr.clone(), &config.server).start();

    // Use 0 to signal that default should be used. This configured the max size that POST endpoints
    // will accept.
    let max_body_size =  if let Some(size) = config.server.max_body_size {
        debug!("Setting max body size to {} bytes", size);
        size
    } else {
        0
    };

    // Set up HTTP server routing. Each endpoint has access to the ApplicationState struct, allowing them to send
    // messages to the RedisActor (which performs task queue operations).
    let config_copy = config.clone();
    let mut http_server = server::new(move || {
        App::with_state(ApplicationState::new(redis_addr.clone(), config_copy.clone()))
            // get a summary of the Ocypod system as a whole, e.g. number of jobs in queues, job states, etc.
            .scope("/info", |info_scope| {
                info_scope
                    // get current server version
                    .resource("/version", |r| r.f(handlers::info::version))

                    // get summary of system/queue information
                    .resource("", |r| r.f(handlers::info::index))
            })

            // run basic health check by pinging Redis
            .resource("/health", |r| r.f(handlers::health::index))

            // get list of job IDs for a given tag
            .resource("/tag/{name}", |r| r.method(http::Method::GET).with(handlers::tag::tagged_jobs))

            .scope("/job", |job_scope| {
                job_scope
                    // get the current status of a job with given ID
                    .resource("/{id}/status", |r| r.method(http::Method::GET).with(handlers::job::status))
                    .resource("/{id}/output", move |r| {
                        // TODO: deprecate this and just use fields endpoint?
                        // get the output field of a job with given ID
                        r.method(http::Method::GET).with(handlers::job::output);

                        // update the output field of a job with given ID
                        if max_body_size > 0 {
                            r.method(http::Method::PUT)
                                .with_config(handlers::job::set_output, |((_, cfg), _)| { cfg.limit(max_body_size); })
                        } else {
                            r.method(http::Method::PUT).with(handlers::job::set_output)
                        }
                    })

                    // update job's last heartbeat date/time
                    .resource("/{id}/heartbeat", |r| r.method(http::Method::PUT).with(handlers::job::heartbeat))

                    .resource("/{id}", move |r| {
                        // get all metadata about a single job with given ID
                        r.method(http::Method::GET).with(handlers::job::index);

                        // update one of more fields (including status) of given job
                        if max_body_size > 0 {
                            r.method(http::Method::PATCH)
                                .with_config(handlers::job::update, |((_, cfg), _)| { cfg.limit(max_body_size); });
                        } else {
                            r.method(http::Method::PATCH).with(handlers::job::update);
                        }

                        // delete a job from the queue DB
                        r.method(http::Method::DELETE).with(handlers::job::delete)
                    })
            })

            .scope("/queue", |queue_scope| {
                queue_scope
                    .resource("/{name}/job", move |r| {
                        // get the next job to work on from given queue
                        r.method(http::Method::GET).with(handlers::queue::next_job);

                        // create a new job on given queue
                        if max_body_size > 0 {
                            r.method(http::Method::POST)
                                .with_config(handlers::queue::create_job, |((_, cfg), _)| { cfg.limit(max_body_size); })
                        } else {
                            r.method(http::Method::POST).with(handlers::queue::create_job)
                        }
                    })

                    // get queue size
                    .resource("/{name}/size", |r| r.method(http::Method::GET).with(handlers::queue::size))

                    .resource("/{name}", |r| {
                        // get current queue settings settings, etc.
                        r.method(http::Method::GET).with(handlers::queue::settings);

                        // create a new queue, or update an existing one with given settings
                        r.method(http::Method::PUT).with(handlers::queue::create_or_update);

                        // delete a queue and all currently queued jobs on it
                        r.method(http::Method::DELETE).with(handlers::queue::delete)
                    })

                    // get a list of all queue names
                    .resource("", |r| r.f(handlers::queue::index))
            })

            // add middleware logger for access log, if required
            .middleware(Logger::default())
    });

    // set number of worker threads if configured, or default to number of logical CPUs
    if let Some(num_workers) = config.server.threads {
        debug!("Using {} HTTP worker threads", num_workers);
        http_server = http_server.workers(num_workers);
    }

    if let Some(dur) = config.server.shutdown_timeout {
        debug!("Setting shutdown timeout to {}", dur);
        http_server = http_server.shutdown_timeout(dur.as_secs() as u16);
    }

    http_server.bind(&http_server_addr)
        .expect("Failed to start HTTP server")
        .start();

    // start HTTP service and actor system
    info!("Starting queue server at: {}", &http_server_addr);
    let _ = sys.run();
}

/// Defines and parses CLI argument for this server.
fn parse_cli_args<'a>() -> clap::ArgMatches<'a> {
    clap::App::new("Ocypod")
        .version(handlers::info::VERSION)
        .arg(clap::Arg::with_name("config")
            .required(false)
            .help("Path to configuration file")
            .index(1))
        .get_matches()
}

/// Parses CLI arguments, finds location of config file, and parses config file into a struct.
fn parse_config_from_cli_args(matches: &clap::ArgMatches) -> config::Config {
    let conf = match matches.value_of("config") {
        Some(config_path) => {
            match config::Config::from_file(config_path) {
                Ok(config) => config,
                Err(msg) => {
                    eprintln!("Failed to parse config file {}: {}", config_path, msg);
                    std::process::exit(1);
                },
            }
        },
        None => {
            warn!("No config file specified, using default config");
            config::Config::default()
        }
    };

    // validate config settings
    if let Some(dur) = &conf.server.shutdown_timeout {
        if dur.as_secs() > std::u16::MAX.into() {
            eprintln!("Maximum shutdown_timeout is {} seconds", std::u16::MAX);
            std::process::exit(1);
        }
    }

    conf
}
