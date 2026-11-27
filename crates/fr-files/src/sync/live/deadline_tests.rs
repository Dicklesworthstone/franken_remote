//! Real UDP/TLS and original folder owners. Only the time left in the
//! handshake or round is shortened; normal path allowances exceed both.
#[allow(dead_code)]
#[path = "../../../tests/native_support/mod.rs"]
mod native_support;

use super::*;
use crate::{
    quic::Configuration,
    receive::Limits,
    sender::{Policy as SendPolicy, Sender},
    session::{Authority, Permission, Policy},
    sync::{HostConfig, HostJob},
};
use asupersync::cx::Cx;
use native_support::{Link, clock, runtime};
use std::{fs, os::unix::fs::DirBuilderExt, path::Path, time::Duration};

const SHORT_US: u64 = 250_000;

fn owners(
    cx: &Cx,
    link: &mut Link,
    destination: &Path,
    lane: fr_transport::quic::files::FilesChannel,
    authority: Authority,
) -> (ControllerJob, Receiver) {
    let directory = DropDirectory::open(
        destination,
        Limits {
            max_file_bytes: 1024,
            max_reserved_bytes: 1024,
            max_transfers: 1,
        },
    )
    .unwrap()
    .with_conflict_policy(ConflictPolicy::KeepBoth);
    let job = ControllerJob::new(41, directory).unwrap();
    let receiver = Receiver::spawn_with_authority(
        cx.clone(),
        &mut link.c,
        lane,
        authority,
        Configuration {
            directory: job.directory(),
            permission: Permission::new(true),
            policy: Policy::conservative(),
            reply_lifetime: Duration::from_secs(1),
        },
    )
    .unwrap();
    (job, receiver)
}

fn clean(
    cx: &Cx,
    link: &mut Link,
    host: &mut HostJob,
    sender: &mut Sender<'_>,
    client: &mut ControllerJob,
    disk: &mut Receiver,
) {
    host.stop();
    sender.cancel(&mut link.h).unwrap();
    client.stop();
    disk.retire(&mut link.c).unwrap();
    let until = clock(cx) + 1_000_000;
    loop {
        assert!(clock(cx) < until);
        let host_cleanup = host.try_finish_cleanup(sender);
        let disk_cleanup = disk.try_finish_cleanup();
        if let (Some(host), Some(disk)) = (host_cleanup, disk_cleanup) {
            host.unwrap();
            disk.unwrap();
            break;
        }
        std::thread::yield_now();
    }
}

#[test]
fn delayed_transport_cannot_deliver_original_folder_acceptance_or_round_start() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        for accepting in [true, false] {
            let root = std::env::temp_dir().join(format!(
                "fr-folder-state-deadline-{}-{accepting}",
                std::process::id()
            ));
            let source = root.join("source");
            let destination = root.join("destination");
            for dir in [&root, &source, &destination] {
                fs::DirBuilder::new().mode(0o700).create(dir).unwrap();
            }
            fs::write(source.join("selected"), b"original").unwrap();
            let mut link = Link::new(&cx).await;
            let (host_lane, client_lane, input, _client_input, authority) =
                link.download(&cx, true).await;
            let until = clock(&cx) + 1_000_000;
            while link.c.usage().retained_send_records != 0
                || link.h.usage().retained_send_records != 0
            {
                assert!(clock(&cx) < until, "attachment setup did not drain");
                link.drive(&cx).await;
            }
            let mut config = HostConfig::open(&source, 41).unwrap();
            config.round_lifetime_us = SHORT_US;
            let mut host = HostJob::new(
                config,
                cx.clone(),
                Authority::from_input(&input),
                Permission::new(true),
                &link.h,
            )
            .unwrap();
            let mut sender =
                Sender::owning(cx.clone(), &link.h, host_lane, SendPolicy::default()).unwrap();
            let (mut client, mut disk) =
                owners(&cx, &mut link, &destination, client_lane, authority);
            if accepting {
                client.advertised_until = Some(clock(&cx) + SHORT_US);
            }
            let until = clock(&cx) + 1_000_000;
            loop {
                assert!(clock(&cx) < until, "folder record never reached transport");
                host.service(&mut sender, &mut link.h, || true).unwrap();
                if !accepting && host.status().state.phase == SyncPhase::Scanning {
                    assert_ne!(link.h.usage().retained_send_records, 0);
                    break;
                }
                client.service(&mut disk, &mut link.c, || true).unwrap();
                if accepting && client.pending.as_ref().is_some_and(|p| p.admitted) {
                    assert_eq!(client.status().state.phase, SyncPhase::Accepted);
                    assert_ne!(link.c.usage().retained_send_records, 0);
                    break;
                }
                link.drive(&cx).await;
            }

            // The original record has no ACK and neither owner gets another
            // turn. A regular reliable record still has at least one second of
            // path allowance here. Only the job state expires at this tick.
            asupersync::time::sleep(cx.now(), Duration::from_micros(SHORT_US + 1)).await;
            let sending = if accepting { &mut link.c } else { &mut link.h };
            sending.tick(&cx, || true).unwrap();
            assert_eq!(sending.usage().retained_send_records, 0);
            assert!(!sending.is_closed());
            if accepting {
                assert_eq!(
                    disk.sync_clock(&link.c),
                    Err(crate::quic::Error::Transport(
                        fr_transport::quic::Error::SendExpired
                    ))
                );
            } else {
                assert!(sender.sync_lane().send_expired(&link.h));
            }
            assert_eq!(host.status().rounds_settled, 0);
            assert_eq!(client.status().confirmed_publications, 0);
            assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
            link.control_progress(&cx).await;

            clean(
                &cx,
                &mut link,
                &mut host,
                &mut sender,
                &mut client,
                &mut disk,
            );
            drop((disk, client, sender, host));
            fs::remove_dir_all(root).unwrap();
        }
    });
}
