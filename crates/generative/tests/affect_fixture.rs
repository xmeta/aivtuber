//! #84 S1: accuracy and latency of the text-to-affect classifier on the
//! labelled synthetic fixture (examples/affect/affect-lines.json). The `dev`
//! split was used while building the lexicon; `test` is the reported figure.
//! Run with `--nocapture` to see the report.

use aivtuber_generative::{AffectClass, classify_affect};
use std::time::Instant;

struct Line {
    split: String,
    label: AffectClass,
    text: String,
}

fn lines() -> Vec<Line> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/affect/affect-lines.json"
    );
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("json");
    doc["lines"]
        .as_array()
        .expect("lines")
        .iter()
        .map(|line| Line {
            split: line["split"].as_str().expect("split").to_owned(),
            label: AffectClass::parse(line["label"].as_str().expect("label")).expect("class"),
            text: line["text"].as_str().expect("text").to_owned(),
        })
        .collect()
}

struct Report {
    accuracy: f64,
    false_non_neutral: f64,
}

fn report(split: &str, lines: &[Line]) -> Report {
    let rows: Vec<_> = lines.iter().filter(|l| l.split == split).collect();
    let predicted: Vec<_> = rows
        .iter()
        .map(|l| classify_affect(&l.text).class)
        .collect();
    let correct = rows
        .iter()
        .zip(&predicted)
        .filter(|(l, p)| l.label == **p)
        .count();
    println!("== {split}: {} lines ==", rows.len());
    for class in AffectClass::ALL {
        let tp = rows
            .iter()
            .zip(&predicted)
            .filter(|(l, p)| l.label == class && **p == class)
            .count();
        let predicted_n = predicted.iter().filter(|p| **p == class).count();
        let actual_n = rows.iter().filter(|l| l.label == class).count();
        let ratio = |n: usize, d: usize| {
            if d == 0 {
                f64::NAN
            } else {
                n as f64 / d as f64
            }
        };
        println!(
            "{:9} n={actual_n:2} precision={:.2} recall={:.2}",
            class.as_str(),
            ratio(tp, predicted_n),
            ratio(tp, actual_n)
        );
    }
    for (l, p) in rows.iter().zip(&predicted) {
        if l.label != *p {
            println!(
                "  MISS label={} got={} text={}",
                l.label.as_str(),
                p.as_str(),
                l.text
            );
        }
    }
    let neutral: Vec<_> = rows
        .iter()
        .zip(&predicted)
        .filter(|(l, _)| l.label == AffectClass::Neutral)
        .collect();
    let false_non_neutral = neutral
        .iter()
        .filter(|(_, p)| **p != AffectClass::Neutral)
        .count() as f64
        / neutral.len() as f64;
    let accuracy = correct as f64 / rows.len() as f64;
    println!("accuracy={accuracy:.3} false_non_neutral_on_neutral={false_non_neutral:.3}");
    Report {
        accuracy,
        false_non_neutral,
    }
}

#[test]
fn fixture_covers_every_class_in_both_splits() {
    let lines = lines();
    assert!(lines.len() >= 60);
    for split in ["dev", "test"] {
        for class in AffectClass::ALL {
            assert!(
                lines.iter().any(|l| l.split == split && l.label == class),
                "{split} lacks {}",
                class.as_str()
            );
        }
    }
}

#[test]
fn accuracy_meets_the_s1_targets_on_the_held_out_split() {
    let lines = lines();
    report("dev", &lines);
    let test = report("test", &lines);
    assert!(test.accuracy >= 0.7, "accuracy {}", test.accuracy);
    assert!(
        test.false_non_neutral <= 0.15,
        "false non-neutral {}",
        test.false_non_neutral
    );
}

#[test]
fn classification_cost_per_phrase() {
    let lines = lines();
    let mut micros = Vec::new();
    for _ in 0..20 {
        for line in &lines {
            let started = Instant::now();
            std::hint::black_box(classify_affect(std::hint::black_box(&line.text)));
            micros.push(started.elapsed().as_secs_f64() * 1e6);
        }
    }
    micros.sort_by(f64::total_cmp);
    let median = micros[micros.len() / 2];
    let p99 = micros[micros.len() * 99 / 100];
    println!(
        "classify_affect per phrase: median={median:.1}us p99={p99:.1}us n={}",
        micros.len()
    );
    // Generous ceiling for unoptimised CI hosts; the figure above is the point.
    assert!(p99 < 5_000.0, "p99 {p99}us");
}
