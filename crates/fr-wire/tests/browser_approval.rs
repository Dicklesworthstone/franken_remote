use fr_core::limits::ProtocolLimits;
use fr_wire::negotiation::{Capability, Error, Role, browser};

#[test]
fn browser_consent_is_optional_until_the_host_requires_it() {
    let client = browser::offer(ProtocolLimits::ABSOLUTE);
    let optional = browser::select(&client).unwrap();
    assert!(browser::local_approval_selected(&optional));
    assert!(!browser::local_approval_required(&optional));

    let mut host = client.clone();
    host.capabilities
        .iter_mut()
        .find(|capability| capability.name == browser::APPROVAL_CAPABILITY)
        .unwrap()
        .required = true;
    let intersection = browser::intersect(&host, &client).unwrap();
    browser::check_host(&client, &intersection).unwrap();
    let selected = browser::select(&intersection).unwrap();
    assert!(browser::local_approval_selected(&selected));
    assert!(browser::local_approval_required(&selected));

    let mut legacy = client;
    legacy
        .capabilities
        .retain(|capability| capability.name != browser::APPROVAL_CAPABILITY);
    assert_eq!(
        browser::intersect(&host, &legacy),
        Err(Error::RequiredCapability)
    );
    let legacy_intersection = browser::intersect(&legacy, &host).unwrap_err();
    assert_eq!(legacy_intersection, Error::RequiredCapability);
}

#[test]
fn optional_consent_does_not_break_a_host_without_the_extension() {
    let client = browser::offer(ProtocolLimits::ABSOLUTE);
    let mut host = client.clone();
    host.capabilities
        .retain(|capability| capability.name != browser::APPROVAL_CAPABILITY);
    let intersection = browser::intersect(&host, &client).unwrap();
    browser::check_host(&client, &intersection).unwrap();
    let selected = browser::select(&intersection).unwrap();
    assert!(!browser::local_approval_selected(&selected));
    assert!(!browser::local_approval_required(&selected));
}

#[test]
fn consent_helpers_require_the_exact_profile_role_version_and_valid_capabilities() {
    let selected = browser::select(&browser::offer(ProtocolLimits::ABSOLUTE)).unwrap();
    let mut invalid = selected.clone();
    invalid.profile = 1;
    assert!(!browser::local_approval_selected(&invalid));
    invalid = selected.clone();
    invalid.role = Role::RequestControl;
    assert!(!browser::local_approval_selected(&invalid));
    invalid = selected.clone();
    invalid.capabilities[0].version += 1;
    assert!(!browser::local_approval_selected(&invalid));
    invalid = selected;
    invalid.capabilities.insert(
        0,
        Capability {
            name: browser::APPROVAL_CAPABILITY.to_owned(),
            version: browser::APPROVAL_VERSION,
            required: true,
        },
    );
    assert!(!browser::local_approval_selected(&invalid));
    assert!(!browser::local_approval_required(&invalid));
}
