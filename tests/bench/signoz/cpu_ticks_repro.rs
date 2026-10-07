#[path = "../../../guest/arcbox-agent/src/stats.rs"]
mod stats;

#[test]
fn nested_guest_time_is_not_double_counted() {
    // Linux includes the 80 guest ticks in the 100 user ticks.
    let line = "cpu  100 0 0 100 0 0 0 0 80 0\n";
    assert_eq!(stats::parse_cpu_ticks(line), Some((100, 200)));
}
