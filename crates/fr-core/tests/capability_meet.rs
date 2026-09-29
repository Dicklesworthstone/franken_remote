//! The control grant is a meet: requested ∧ host policy ∧ probed executor.
//! Exhaustive over every subset of the eight capabilities (256 × 256 pairs).
use fr_core::input_submission::{Capabilities, Capability};

const ALL: [Capability; 8] = [
    Capability::Keys,
    Capability::Repeat,
    Capability::Absolute,
    Capability::Buttons,
    Capability::Relative,
    Capability::PixelScroll,
    Capability::LineScroll,
    Capability::Text,
];

fn subset(bits: u8) -> Capabilities {
    ALL.iter()
        .enumerate()
        .filter(|(i, _)| bits & (1 << i) != 0)
        .fold(Capabilities::default(), |set, (_, &c)| set.with(c))
}

#[test]
fn meet_is_the_intersection_lattice() {
    for a in 0..=u8::MAX {
        let x = subset(a);
        assert_eq!(x.meet(x), x, "idempotent");
        assert_eq!(x.meet(Capabilities::default()), Capabilities::default());
        assert_eq!(x.meet(subset(u8::MAX)), x, "the full set is the identity");
        for b in 0..=u8::MAX {
            let y = subset(b);
            let m = x.meet(y);
            assert_eq!(m, y.meet(x), "commutative");
            assert_eq!(m, subset(a & b), "exactly the shared capabilities");
            assert!(x.contains_all(m) && y.contains_all(m), "never grows");
            // Monotone: a smaller input never yields a larger grant.
            let smaller = subset(a & b);
            assert!(m.contains_all(smaller.meet(y)));
        }
    }
    // Associative on a spread of triples (every pair above is exhaustive).
    for (a, b, c) in [(0xff, 0x0f, 0x3c), (0x55, 0xaa, 0xff), (0x4f, 0xef, 0x7f)] {
        let (x, y, z) = (subset(a), subset(b), subset(c));
        assert_eq!(x.meet(y).meet(z), x.meet(y.meet(z)));
    }
}

#[test]
fn a_missing_optional_capability_leaves_the_required_core() {
    let core = Capabilities::default()
        .with(Capability::Keys)
        .with(Capability::Repeat)
        .with(Capability::Absolute)
        .with(Capability::Buttons);
    let wanted = core.with(Capability::LineScroll);
    // An executor without line scrolling (several X screens, wheel buttons
    // unmapped) still grants the whole core; the wheel alone is unavailable.
    let grant = wanted.meet(core);
    assert!(grant.contains_all(core));
    assert!(!grant.contains(Capability::LineScroll));
    assert_eq!(wanted.meet(wanted), wanted);
}
