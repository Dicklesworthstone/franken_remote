use super::*;
use crate::options::SavedSelection;
use std::sync::atomic::{AtomicUsize, Ordering};
fn file() -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "fr-saved-selection-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    root.join("hosts.json")
}
fn target(file: &std::path::Path, saved: bool) -> Target {
    Target {
        saved: saved.then(|| SavedSelection {
            file: Some(file.to_owned()),
        }),
        node: "office".into(),
        by_name: false,
        roots: PathBuf::from("/unused/roots.pem"),
        port: 8443,
        ipv6: false,
    }
}
#[test]
fn save_select_list_and_forget_use_the_real_local_file() {
    let path = file();
    let options = SavedHostOptions {
        action: SavedHostAction::Save {
            alias: "office".into(),
            node: "n-office".into(),
            port: 9443,
        },
        file: Some(path.clone()),
    };
    assert!(local(&options, true).unwrap().is_none());
    let output: serde_json::Value =
        serde_json::from_str(&save(&options, "n-office", "office.fixture.ts.net", true).unwrap())
            .unwrap();
    assert_eq!(output["selection_only"], true);
    assert_eq!(output["fresh_lookup_required"], true);
    assert_eq!(output["hosts"][0]["port"], 9443);
    let selected = select(&target(&path, true)).unwrap();
    assert_eq!(selected.node, "n-office");
    assert_eq!(selected.port, 9443);
    assert!(matches!(
        selected.selector(),
        fr_tailnet::PeerSelector::Pinned {
            stable_id: "n-office",
            certificate_name: "office.fixture.ts.net",
        }
    ));
    let ordinary = select(&target(&path, false)).unwrap();
    assert_eq!(ordinary.port, 8443);
    assert!(matches!(
        ordinary.selector(),
        fr_tailnet::PeerSelector::StableId("office")
    ));
    let listed = local(
        &SavedHostOptions {
            action: SavedHostAction::List,
            file: Some(path.clone()),
        },
        true,
    )
    .unwrap()
    .unwrap();
    let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
    assert_eq!(listed["changed"], false);
    assert!(listed["durable"].is_null());
    local(
        &SavedHostOptions {
            action: SavedHostAction::Forget {
                alias: "office".into(),
            },
            file: Some(path.clone()),
        },
        false,
    )
    .unwrap();
    assert!(matches!(
        select(&target(&path, true)),
        Err(Failure {
            code: "saved_host_not_found",
            ..
        })
    ));
}
#[test]
fn list_is_read_only_and_a_changed_lookup_is_never_saved() {
    let path = file();
    let list = SavedHostOptions {
        action: SavedHostAction::List,
        file: Some(path.clone()),
    };
    assert!(local(&list, true).unwrap().is_some());
    assert!(!path.exists());
    let save_options = SavedHostOptions {
        action: SavedHostAction::Save {
            alias: "office".into(),
            node: "n-office".into(),
            port: 8443,
        },
        file: Some(path.clone()),
    };
    assert!(
        save(
            &save_options,
            "n-replacement",
            "office.fixture.ts.net",
            true
        )
        .is_err()
    );
    assert!(!path.exists());
}
#[test]
fn defaults_and_durability_do_not_claim_more_than_is_known() {
    assert_eq!(
        default_path(Some("/config".into()), Some("/home/user".into())).unwrap(),
        PathBuf::from("/config/franken_remote/hosts.json")
    );
    assert_eq!(
        default_path(None, Some("/home/user".into())).unwrap(),
        PathBuf::from("/home/user/.config/franken_remote/hosts.json")
    );
    assert_eq!(
        default_path(Some("relative".into()), Some("/home/user".into())),
        Err(Error::InvalidPath)
    );
    assert_eq!(default_path(None, None), Err(Error::InvalidPath));
    let output: serde_json::Value =
        serde_json::from_str(&render(&SavedPeers::default(), "save", Some(false), true)).unwrap();
    assert_eq!(output["changed"], true);
    assert_eq!(output["durable"], false);
    let error = failure(Error::Data(SavedPeerError::InvalidDocument));
    assert_eq!(error.code, "saved_hosts_invalid");
}
