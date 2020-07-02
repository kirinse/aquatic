use std::time::Duration;
use std::fs::File;
use std::io::Read;
use std::sync::Arc;
use std::thread::Builder;

use anyhow::Context;
use native_tls::{Identity, TlsAcceptor};
use parking_lot::Mutex;
use privdrop::PrivDrop;

pub mod common;
pub mod config;
pub mod handler;
pub mod network;
pub mod protocol;
pub mod tasks;

use common::*;
use config::Config;


// almost identical to ws version
pub fn run(config: Config) -> anyhow::Result<()> {
    let opt_tls_acceptor = create_tls_acceptor(&config)?;

    let state = State::default();

    let (request_channel_sender, request_channel_receiver) = ::flume::unbounded();

    let mut out_message_senders = Vec::new();

    let socket_worker_statuses: SocketWorkerStatuses = {
        let mut statuses = Vec::new();

        for _ in 0..config.socket_workers {
            statuses.push(None);
        }

        Arc::new(Mutex::new(statuses))
    };

    for i in 0..config.socket_workers {
        let config = config.clone();
        let socket_worker_statuses = socket_worker_statuses.clone();
        let request_channel_sender = request_channel_sender.clone();
        let opt_tls_acceptor = opt_tls_acceptor.clone();

        let (response_channel_sender, response_channel_receiver) = ::flume::unbounded();

        out_message_senders.push(response_channel_sender);

        Builder::new().name(format!("socket-{:02}", i + 1)).spawn(move || {
            network::run_socket_worker(
                config,
                i,
                socket_worker_statuses,
                request_channel_sender,
                response_channel_receiver,
                opt_tls_acceptor
            );
        })?;
    }

    // Wait for socket worker statuses. On error from any, quit program.
    // On success from all, drop privileges if corresponding setting is set
    // and continue program.
    loop {
        ::std::thread::sleep(::std::time::Duration::from_millis(10));

        if let Some(statuses) = socket_worker_statuses.try_lock(){
            for opt_status in statuses.iter(){
                match opt_status {
                    Some(Err(err)) => {
                        return Err(::anyhow::anyhow!(err.to_owned()));
                    },
                    _ => {},
                }
            }

            if statuses.iter().all(Option::is_some){
                if config.privileges.drop_privileges {
                    PrivDrop::default()
                        .chroot(config.privileges.chroot_path.clone())
                        .user(config.privileges.user.clone())
                        .apply()
                        .context("Couldn't drop root privileges")?;
                }

                break
            }
        }
    }

    let response_channel_sender = ResponseChannelSender::new(out_message_senders);

    {
        let config = config.clone();
        let state = state.clone();

        Builder::new().name("request".to_string()).spawn(move || {
            handler::run_request_worker(
                config,
                state,
                request_channel_receiver,
                response_channel_sender,
            );
        })?;
    }

    loop {
        ::std::thread::sleep(Duration::from_secs(config.cleaning.interval));

        tasks::clean_torrents(&state);
    }
}


// identical to ws version
pub fn create_tls_acceptor(
    config: &Config,
) -> anyhow::Result<Option<TlsAcceptor>> {
    if config.network.use_tls {
        let mut identity_bytes = Vec::new();
        let mut file = File::open(&config.network.tls_pkcs12_path)
            .context("Couldn't open pkcs12 identity file")?;

        file.read_to_end(&mut identity_bytes)
            .context("Couldn't read pkcs12 identity file")?;

        let identity = Identity::from_pkcs12(
            &mut identity_bytes,
            &config.network.tls_pkcs12_password
        ).context("Couldn't parse pkcs12 identity file")?;

        let acceptor = TlsAcceptor::new(identity)
            .context("Couldn't create TlsAcceptor from pkcs12 identity")?;

        Ok(Some(acceptor))
    } else {
        Ok(None)
    }
}