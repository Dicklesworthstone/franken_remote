use fr_core::limits::ProtocolLimits;
use fr_wire::{
    display,
    negotiation::{Role, browser},
};

#[test]
fn browser_display_choice_is_optional_and_requires_exact_positive_selection() {
    let client = browser::offer(ProtocolLimits::ABSOLUTE);
    let capability = client
        .capabilities
        .iter()
        .find(|capability| capability.name == display::CAPABILITY)
        .unwrap();
    assert_eq!(capability.version, display::VERSION);
    assert!(!capability.required);
    let selected = browser::select(&browser::intersect(&client, &client).unwrap()).unwrap();
    assert!(browser::display_selection_selected(&selected));
    let mut wrong = selected.clone();
    wrong.profile = 1;
    assert!(!browser::display_selection_selected(&wrong));
    wrong = selected.clone();
    wrong.role = Role::RequestControl;
    assert!(!browser::display_selection_selected(&wrong));
    wrong = selected;
    wrong
        .capabilities
        .iter_mut()
        .find(|capability| capability.name == display::CAPABILITY)
        .unwrap()
        .version += 1;
    assert!(!browser::display_selection_selected(&wrong));
}

#[test]
fn legacy_peer_without_display_selection_retains_its_original_default_path() {
    let current = browser::offer(ProtocolLimits::ABSOLUTE);
    let mut legacy = current.clone();
    legacy
        .capabilities
        .retain(|capability| capability.name != display::CAPABILITY);
    for (host, client) in [(&current, &legacy), (&legacy, &current)] {
        let offer = browser::intersect(host, client).unwrap();
        browser::check_host(client, &offer).unwrap();
        let selection = browser::select(&offer).unwrap();
        browser::check_selection(&selection, &offer).unwrap();
        assert!(!browser::display_selection_selected(&selection));
    }
}
