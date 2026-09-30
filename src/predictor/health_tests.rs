// SPDX-License-Identifier: MIT
use super::*;
use crate::predictor::data;
fn fixture() -> Value {
    serde_json::from_str(include_str!("../../tests/fixtures/health-parity.json")).unwrap()
}
#[test]
fn real_notice_corpus_matches_independent_python_policy() {
    let fixture = fixture();
    let mut notices = Vec::new();
    for row in fixture["notices"].as_array().unwrap() {
        let parsed = parse_notice(
            row["text"].as_str().unwrap(),
            row["known"].as_f64().unwrap(),
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(&parsed).unwrap(),
            row["expected"],
            "{}",
            parsed.number
        );
        notices.push(parsed);
    }
    let (mut outages, mut reviews) = resolve(&notices, fixture["as_of"].as_f64().unwrap()).unwrap();
    outages.sort_by(|a, b| a.root_notice.cmp(&b.root_notice));
    reviews.sort_by(|a, b| a.number.cmp(&b.number));
    let mut expected: Vec<Outage> = serde_json::from_value(fixture["outages"].clone()).unwrap();
    expected.sort_by(|a, b| a.root_notice.cmp(&b.root_notice));
    let mut expected_reviews: Vec<Notice> =
        serde_json::from_value(fixture["reviews"].clone()).unwrap();
    expected_reviews.sort_by(|a, b| a.number.cmp(&b.number));
    assert_eq!(
        serde_json::to_value(outages).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
    assert_eq!(
        serde_json::to_value(reviews).unwrap(),
        serde_json::to_value(expected_reviews).unwrap()
    );
}
#[test]
fn health_guard_matches_python_bytes_across_22_failure_and_lifecycle_cases() {
    let bytes = include_bytes!("../../tests/fixtures/working-camera.cep");
    for case in fixture()["cases"].as_array().unwrap() {
        let state: Snapshot = serde_json::from_value(case["state"].clone()).unwrap();
        let (guarded, report) = guard(
            bytes,
            case["training"].as_f64().unwrap(),
            &state,
            case["now"].as_f64().unwrap(),
        )
        .unwrap();
        assert_eq!(
            crate::camera::hash(&guarded),
            case["sha256"].as_str().unwrap(),
            "{}",
            case["name"]
        );
        assert_eq!(
            report["available_prns_by_week"], case["available"],
            "{}",
            case["name"]
        );
        assert_eq!(
            report["upload_allowed"], case["allowed"],
            "{}",
            case["name"]
        );
        validation::validate(&guarded).unwrap();
    }
}
#[test]
fn utc_dates_and_future_notice_timestamps_are_causal() {
    let gps = utc_gps(
        DateTime::parse_from_rfc3339("2027-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
    )
    .unwrap();
    assert_eq!(gps, data::epoch("2027-01-01T00:00:18").unwrap());
}
#[test]
fn observed_discontinuity_is_detected_from_self_contained_fixture() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/orbit-discontinuity.json"
    ))
    .unwrap();
    let samples: Vec<data::Sample> = serde_json::from_value(fixture["samples"].clone()).unwrap();
    let frames = Frames::read(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/orbit-discontinuity-eop.txt"),
    )
    .unwrap();
    let events = detect(&samples, &frames, samples.last().unwrap().time).unwrap();
    assert_eq!(
        events.iter().map(|e| e.prn).collect::<BTreeSet<_>>(),
        BTreeSet::from([15])
    );
    let fifteen = events.iter().find(|e| e.prn == 15).unwrap();
    assert_eq!(
        fifteen.first_gps,
        fixture["expected_first_gps"].as_f64().unwrap()
    );
    assert!(fifteen.peak_innovation_m > 4000.);
    assert_eq!(
        serde_json::to_value(merge_events(&events, &events)).unwrap(),
        serde_json::to_value(&events).unwrap()
    );
    let before = detect(
        &samples,
        &frames,
        fixture["expected_first_gps"].as_f64().unwrap() - 900.,
    )
    .unwrap();
    assert!(before.is_empty());
}
