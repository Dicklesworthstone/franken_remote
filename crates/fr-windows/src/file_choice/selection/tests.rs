use crate::file_choice::{Error, MAX_PATH_UNITS, MAX_SELECTED_FILES, selected_paths};

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

fn single(path: &str) -> (Vec<u16>, usize) {
    let mut units = wide(path);
    let offset = units
        .iter()
        .rposition(|unit| *unit == u16::from(b'\\'))
        .map_or(0, |index| index + 1);
    units.push(0);
    (units, offset)
}

fn multiple(directory: &str, names: &[&str]) -> (Vec<u16>, usize) {
    let mut units = wide(directory);
    units.push(0);
    let offset = units.len();
    for name in names {
        units.extend(name.encode_utf16());
        units.push(0);
    }
    units.push(0);
    (units, offset)
}

#[test]
fn one_selection_preserves_the_full_path_and_unicode_without_a_second_name() {
    for path in [r"C:\draft.txt", r"c:\Work space\Résumé 🦀 final.txt"] {
        let (mut units, offset) = single(path);
        assert_eq!(selected_paths(&units, offset), Ok(vec![wide(path)]));
        units.resize(MAX_PATH_UNITS, 0);
        assert_eq!(selected_paths(&units, offset), Ok(vec![wide(path)]));
    }
}

#[test]
fn multiple_names_preserve_order_spaces_and_surrogate_pairs() {
    let (units, offset) = multiple(r"D:\Work 🦀", &["Résumé final.txt", "🦀.png", ".gitignore"]);
    assert_eq!(
        selected_paths(&units, offset),
        Ok(vec![
            wide(r"D:\Work 🦀\Résumé final.txt"),
            wide(r"D:\Work 🦀\🦀.png"),
            wide(r"D:\Work 🦀\.gitignore"),
        ])
    );
}

#[test]
fn root_directory_and_an_existing_separator_are_not_duplicated() {
    for directory in [r"C:\", r"C:\Work\"] {
        let (units, offset) = multiple(directory, &["a", "b"]);
        assert_eq!(
            selected_paths(&units, offset),
            Ok(vec![
                wide(&format!("{directory}a")),
                wide(&format!("{directory}b"))
            ])
        );
    }
}

#[test]
fn every_name_must_fit_the_native_offset_contract() {
    for (units, offset) in [single(r"C:\work\a"), multiple(r"C:\work", &["a", "b"])] {
        for wrong in [0, offset - 1, offset + 1, units.len(), usize::MAX] {
            assert_eq!(selected_paths(&units, wrong), Err(Error::InvalidPath));
        }
        assert!(selected_paths(&units, offset).is_ok());
    }
}

#[test]
fn names_are_never_interpreted_as_paths_or_rewritten() {
    for name in [
        "",
        ".",
        "..",
        "a\\b",
        "a/b",
        "\\root",
        "C:relative",
        "C:\\absolute",
        "a:stream",
        "trailing.",
        "trailing ",
        "*.txt",
        "a?b",
        "a\"b",
        "a<b",
        "a>b",
        "a|b",
        "a\nb",
        "NUL",
        "aux.txt",
        "CON .txt",
        "COM1.txt",
        "lpt9",
        "COM¹",
        "LPT².txt",
    ] {
        let (units, offset) = multiple(r"C:\safe", &["valid.txt", name]);
        assert_eq!(
            selected_paths(&units, offset),
            Err(Error::InvalidPath),
            "{name:?}"
        );
    }
}

#[test]
fn roots_cannot_escape_into_relative_network_or_device_namespaces() {
    for root in [
        "",
        "C:",
        "C:relative",
        "relative",
        "/tmp",
        r"\rooted",
        r"\\server\share",
        r"\\?\C:\work",
        r"\\.\C:\work",
        r"C:\..\work",
        r"C:\.\work",
        r"C:\work.\next",
        r"C:\work \next",
        r"C:\\work",
        r"C:\NUL\work",
        r"C:\work/next",
        r"1:\work",
    ] {
        let (units, offset) = multiple(root, &["a", "b"]);
        assert_eq!(
            selected_paths(&units, offset),
            Err(Error::InvalidPath),
            "{root:?}"
        );
    }
}

#[test]
fn single_path_uses_the_same_safe_components_as_multiple_selection() {
    for path in [
        r"C:\work\..\a",
        r"C:\a:stream",
        r"C:\NUL.txt",
        r"C:\trailing.",
        r"C:\trailing ",
        r"\\server\share\a",
        r"\\?\C:\a",
        r"C:\\a",
        r"C:\work\",
        r"C:\",
    ] {
        let (units, offset) = single(path);
        assert_eq!(
            selected_paths(&units, offset),
            Err(Error::InvalidPath),
            "{path:?}"
        );
    }
}

#[test]
fn incomplete_and_hidden_suffixes_refuse_the_whole_batch() {
    let (mut units, offset) = multiple(r"C:\safe", &["a", "b"]);
    units.pop();
    assert_eq!(selected_paths(&units, offset), Err(Error::InvalidPath));
    units.pop();
    assert_eq!(selected_paths(&units, offset), Err(Error::InvalidPath));

    let (mut units, offset) = multiple(r"C:\safe", &["a", "b"]);
    units.extend(wide("hidden"));
    units.push(0);
    assert_eq!(selected_paths(&units, offset), Err(Error::InvalidPath));

    let (mut units, offset) = single(r"C:\safe\a");
    units.extend(wide("hidden"));
    units.push(0);
    assert_eq!(selected_paths(&units, offset), Err(Error::InvalidPath));
}

#[test]
fn empty_or_single_name_multiform_and_duplicate_names_are_refused() {
    for names in [vec![], vec!["a"], vec!["a", "a"], vec!["a", "", "b"]] {
        let (units, offset) = multiple(r"C:\safe", &names);
        assert_eq!(selected_paths(&units, offset), Err(Error::InvalidPath));
    }
    assert_eq!(selected_paths(&[], 0), Err(Error::InvalidPath));
    assert_eq!(selected_paths(&[0, 0], 0), Err(Error::InvalidPath));
}

#[test]
fn invalid_utf16_anywhere_refuses_every_selection() {
    for invalid in [0xd800, 0xdc00] {
        let (mut units, offset) = single(r"C:\work\a");
        units[offset] = invalid;
        assert_eq!(selected_paths(&units, offset), Err(Error::InvalidPath));
        let (mut units, offset) = multiple(r"C:\work", &["a", "b"]);
        units[3] = invalid;
        assert_eq!(selected_paths(&units, offset), Err(Error::InvalidPath));
        let (mut units, offset) = multiple(r"C:\work", &["a", "b"]);
        units[offset + 2] = invalid;
        assert_eq!(selected_paths(&units, offset), Err(Error::InvalidPath));
    }
}

#[test]
fn exactly_eight_files_are_accepted_and_a_ninth_never_becomes_a_partial_batch() {
    let names: Vec<String> = (0..=MAX_SELECTED_FILES)
        .map(|index| format!("{index}.txt"))
        .collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let (units, offset) = multiple(r"C:\safe", &refs[..MAX_SELECTED_FILES]);
    assert_eq!(
        selected_paths(&units, offset).unwrap().len(),
        MAX_SELECTED_FILES
    );
    let (units, offset) = multiple(r"C:\safe", &refs);
    assert_eq!(selected_paths(&units, offset), Err(Error::InvalidPath));
}

#[test]
fn native_input_and_expanded_paths_have_separate_fixed_budgets() {
    let (mut units, offset) = single(r"C:\a");
    units.resize(MAX_PATH_UNITS + 1, 0);
    assert_eq!(selected_paths(&units, offset), Err(Error::InvalidPath));

    let directory = format!("C:\\{}", "x".repeat(MAX_PATH_UNITS / 2));
    let (units, offset) = multiple(&directory, &["a", "b"]);
    assert!(units.len() < MAX_PATH_UNITS);
    assert_eq!(selected_paths(&units, offset), Err(Error::InvalidPath));
}

#[test]
fn exact_expanded_budget_is_accepted_without_truncating_the_last_name() {
    let directory = format!("C:\\{}", "x".repeat(MAX_PATH_UNITS / 2 - 7));
    let (units, offset) = multiple(&directory, &["aa", "bb"]);
    let paths = selected_paths(&units, offset).unwrap();
    assert_eq!(
        paths.iter().map(|path| path.len() + 1).sum::<usize>(),
        MAX_PATH_UNITS
    );
    assert!(paths[0].ends_with(&wide("aa")));
    assert!(paths[1].ends_with(&wide("bb")));
}

#[test]
fn every_prefix_of_a_complete_native_multi_selection_is_rejected() {
    let (units, offset) = multiple(r"C:\Work 🦀", &["one.txt", "Résumé two.txt", "🦀.png"]);
    for length in 0..units.len() {
        assert_eq!(
            selected_paths(&units[..length], offset),
            Err(Error::InvalidPath),
            "{length}"
        );
    }
    assert_eq!(selected_paths(&units, offset).unwrap().len(), 3);
}
