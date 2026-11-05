use codex_superagent_extension::core::thread_rules::is_ticket_name;
use codex_superagent_extension::core::thread_rules::leading_ticket_name;
use pretty_assertions::assert_eq;

#[test]
fn ticket_names_keep_working_past_t999() {
    let generated = ["t001_fix", "t999_fix", "t1000_fix", "t12345_a_b"];
    let not_names = ["t01_fix", "t1000_", "x1000_fix", "t1000fix", "t1000_Fix"];
    assert_eq!(
        generated.map(is_ticket_name),
        [true, true, true, true],
        "{generated:?}"
    );
    assert_eq!(
        not_names.map(is_ticket_name),
        [false, false, false, false, false],
        "{not_names:?}"
    );
    assert_eq!(
        leading_ticket_name("t1000_fix_tests：再补一个用例"),
        Some("t1000_fix_tests")
    );
}
