//! Tests of the report period that Home Assistant sets on
//! `set/report_period`. The core runs in the test shell against the fake
//! broker.

use alloc::{string::String, vec::Vec};

use super::{flows::house, shell::Shell, start};
use crate::{
    Event, PowerSource,
    features::{
        connection::Link,
        power::{self, IDLE_TIMEOUT, ReportPeriod},
    },
};

/// The topic of the report period.
const PERIOD: &str = "dashboard/hall/set/report_period";

/// The status topic of the dashboard.
const STATUS: &str = "dashboard/hall/status";

/// Returns the period of `minutes`.
fn minutes(minutes: u16) -> ReportPeriod {
    ReportPeriod::of_minutes(minutes).expect("a valid period")
}

/// Starts the house on battery, and sleeps after the idle time. Clears the
/// logs of the shell.
fn asleep(shell: &mut Shell) {
    shell.send(start(PowerSource::Battery));
    shell.advance(IDLE_TIMEOUT);
    assert_sleeps(shell);
    shell.take_log();
    shell.take_logs();
}

/// Asserts that a `Sleep` request waits with the radio off.
#[track_caller]
fn assert_sleeps(shell: &Shell) {
    assert_eq!(
        power::sleep_step(&shell.model),
        Some(power::SleepStep::Sleep)
    );
    assert!(shell.sleep.is_some());
    assert!(!shell.radio.on);
}

/// Returns the number of reports: the `reporting` statuses that the broker
/// got.
fn reports(shell: &Shell) -> usize {
    shell
        .broker
        .published
        .iter()
        .filter(|publish| publish.topic == STATUS && publish.payload == b"reporting")
        .count()
}

/// Answers `count` checks, and asserts that none of them reports.
#[track_caller]
fn checks_with_no_report(shell: &mut Shell, count: usize) {
    let before = reports(shell);
    for check in 0..count {
        shell.check();
        assert_eq!(reports(shell), before, "check {check}");
        assert_sleeps(shell);
    }
}

/// Answers one check, and asserts that it reports and that the device then
/// sleeps again.
#[track_caller]
fn check_reports(shell: &mut Shell) {
    let before = reports(shell);
    shell.check();
    assert_eq!(reports(shell), before + 1);
    assert!(!shell.model.power.is_reporting());
    assert_eq!(shell.broker.retained(STATUS), Some("asleep"));
    assert_sleeps(shell);
}

/// Returns the lines of the log about the report period.
fn period_lines(logs: &[String]) -> Vec<&str> {
    logs.iter()
        .map(String::as_str)
        .filter(|line| line.contains("report period"))
        .collect()
}

#[test]
fn a_retained_period_at_the_connect_sets_the_period_of_the_reports() {
    let mut shell = house();
    shell.broker.retain(PERIOD, b"5");
    shell.send(start(PowerSource::Battery));
    assert_eq!(shell.model.connection.link(), Link::Up);
    assert_eq!(shell.model.power.report_period(), minutes(5));
    assert_eq!(
        period_lines(&shell.take_logs()),
        ["debug device: Home Assistant set the report period to 5 min"]
    );
    shell.advance(IDLE_TIMEOUT);
    shell.take_logs();

    // The 5th check reports. The retained message comes again at the
    // connect of the report, and the same period changes nothing.
    checks_with_no_report(&mut shell, 4);
    check_reports(&mut shell);
    assert_eq!(period_lines(&shell.take_logs()), Vec::<&str>::new());
    checks_with_no_report(&mut shell, 4);
    check_reports(&mut shell);
}

#[test]
fn a_period_of_0_means_no_report_after_many_checks() {
    let mut shell = house();
    shell.broker.retain(PERIOD, b"0");
    asleep(&mut shell);
    assert_eq!(shell.model.power.report_period(), ReportPeriod::Off);

    // 1000 checks are more than the longest period.
    checks_with_no_report(&mut shell, 1000);
    assert_eq!(reports(&shell), 0);
    assert_eq!(shell.take_logs(), Vec::<String>::new());

    // A tap still wakes the device.
    shell.send(Event::Tapped(None));
    assert!(!shell.model.power.is_asleep());
}

#[test]
fn a_period_set_while_asleep_applies_at_the_next_report() {
    let mut shell = house();
    asleep(&mut shell);
    assert_eq!(shell.model.power.report_period(), ReportPeriod::DEFAULT);

    // The device has no session while it sleeps, so the broker keeps the
    // new period. The next report, at the default period, gets it.
    shell.broker.retain(PERIOD, b"10");
    checks_with_no_report(&mut shell, 59);
    check_reports(&mut shell);
    assert_eq!(shell.model.power.report_period(), minutes(10));
    assert!(
        period_lines(&shell.take_logs())
            .contains(&"debug device: Home Assistant set the report period to 10 min")
    );

    // The report finished, and the next sleep counts to the new period.
    checks_with_no_report(&mut shell, 9);
    check_reports(&mut shell);

    // The period 0 also arrives in a report. The report finishes, and then
    // no check reports.
    shell.broker.retain(PERIOD, b"0");
    checks_with_no_report(&mut shell, 9);
    check_reports(&mut shell);
    assert_eq!(shell.model.power.report_period(), ReportPeriod::Off);
    assert!(period_lines(&shell.take_logs()).contains(
        &"debug device: Home Assistant set the report period to 0, so the device does not \
          report while it sleeps"
    ));
    checks_with_no_report(&mut shell, 1000);
}

#[test]
fn a_shorter_period_mid_sleep_reports_at_the_next_check() {
    let mut shell = house();
    asleep(&mut shell);
    checks_with_no_report(&mut shell, 30);

    // Only the steps of a sleep and a report have a session, so a new
    // period cannot arrive here from the broker. The test sends the event
    // of the period directly. The count keeps its 30 checks.
    shell.send(Event::Power(power::Event::ReportPeriodSet(minutes(10))));
    assert_sleeps(&shell);
    check_reports(&mut shell);

    // The next sleep counts from 0.
    checks_with_no_report(&mut shell, 9);
    check_reports(&mut shell);
}

#[test]
fn a_longer_period_mid_sleep_keeps_the_count() {
    let mut shell = house();
    asleep(&mut shell);
    checks_with_no_report(&mut shell, 50);

    shell.send(Event::Power(power::Event::ReportPeriodSet(minutes(90))));
    checks_with_no_report(&mut shell, 39);
    check_reports(&mut shell);
}

#[test]
fn the_period_0_mid_sleep_stops_the_reports_and_a_period_again_starts_them() {
    let mut shell = house();
    asleep(&mut shell);
    checks_with_no_report(&mut shell, 50);

    shell.send(Event::Power(power::Event::ReportPeriodSet(
        ReportPeriod::Off,
    )));
    checks_with_no_report(&mut shell, 100);

    // The count went on, so a period shorter than the count reports at the
    // next check.
    shell.send(Event::Power(power::Event::ReportPeriodSet(minutes(60))));
    check_reports(&mut shell);
}

#[test]
fn start_resets_the_period_to_the_default() {
    let mut shell = house();
    shell.send(start(PowerSource::Stand));
    // A live message that the broker does not keep.
    shell.broker.publish(PERIOD, b"15", false);
    shell.run();
    assert_eq!(shell.model.power.report_period(), minutes(15));

    shell.send(start(PowerSource::Stand));
    assert_eq!(shell.model.connection.link(), Link::Up);
    assert_eq!(shell.model.power.report_period(), ReportPeriod::DEFAULT);
}

#[test]
fn an_empty_payload_is_the_default_and_a_payload_that_is_not_valid_is_ignored() {
    let mut shell = house();
    shell.send(start(PowerSource::Stand));
    shell.broker.publish(PERIOD, b"30", true);
    shell.run();
    assert_eq!(shell.model.power.report_period(), minutes(30));
    shell.take_logs();

    for payload in ["721", "725", "-5", "30.5", "half an hour"] {
        shell.broker.publish(PERIOD, payload.as_bytes(), true);
        shell.run();
        assert_eq!(shell.model.power.report_period(), minutes(30), "{payload}");
        assert_eq!(
            shell.take_logs(),
            [alloc::format!(
                "warn device: the report period {payload} on {PERIOD} is not valid, so the \
                 core ignored it"
            )]
        );
    }

    // A user clears the retained message.
    shell.broker.publish(PERIOD, b"", true);
    shell.run();
    assert_eq!(shell.model.power.report_period(), ReportPeriod::DEFAULT);
    assert_eq!(
        shell.take_logs(),
        ["debug device: Home Assistant set the report period to 60 min"]
    );
}
