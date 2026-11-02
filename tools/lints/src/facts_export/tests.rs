// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! End-to-end test: compile a sample crate through the Dylint driver with
//! the export switched on, then check the records it wrote.

use std::{env, fs, path::PathBuf};

use serde_json::Value;

use super::{ENABLE_ENV, SCHEMA};

const LINT: &str = "facts_export";

/// Sample crate covering each record kind and call kind. The pass reports
/// nothing, so the expected compiler output is empty.
const SOURCE: &str = r#"// rustc-env:XKERNEL_FACTS=1
#![allow(dead_code, unused)]

use std::sync::atomic::{AtomicUsize, Ordering};

pub trait Shape {
    fn area(&self) -> u32;
}

#[derive(Clone)]
pub struct Square(u32);

impl Shape for Square {
    fn area(&self) -> u32 {
        self.0 * self.0
    }
}

pub struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {}
}

macro_rules! make_helper {
    ($name:ident) => {
        fn $name() -> u32 {
            7
        }
    };
}

make_helper!(helper);

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn dyn_area(shape: &dyn Shape) -> u32 {
    shape.area()
}

fn static_area(square: &Square) -> u32 {
    square.area()
}

fn generic_area<S: Shape>(shape: &S) -> u32 {
    shape.area()
}

fn apply(f: fn() -> u32) -> u32 {
    f()
}

fn apply_to(f: fn(&Square) -> u32, square: &Square) -> u32 {
    f(square)
}

fn describe(n: u32) -> String {
    format!("{n}")
}

// Coroutine layout is computed during analysis from optimised MIR, so the
// exporter falls back to that MIR for the coroutine body.
async fn ready() -> u32 {
    helper()
}

unsafe fn raw() -> u32 {
    1
}

fn caller() -> u32 {
    let _guard = Guard;
    COUNTER.fetch_add(1, Ordering::Relaxed);
    let total = apply(helper) + static_area(&Square(2)).clone();
    let raw = unsafe { raw() };
    let closure = |x: u32| x + helper();
    total + raw + closure(1)
}

fn main() {
    caller();
}
"#;

/// One facts record; values are compared as JSON.
struct Record(serde_json::Map<String, Value>);

impl Record {
    fn str(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(Value::as_str)
    }

    fn flag(&self, key: &str) -> bool {
        self.0.get(key).and_then(Value::as_bool).unwrap_or(false)
    }
}

/// The facts file of the sample crate. compiletest builds it into the
/// temporary directory with `-o <tmp>/facts_export.stage-id`, so the stem is
/// the test name (see `run_ui` for the same convention for stderr).
fn facts_file() -> PathBuf {
    env::temp_dir().join(format!("{LINT}.facts.jsonl"))
}

#[test]
fn ui() {
    let path = facts_file();
    // A stale file from an earlier run would hide a broken exporter.
    let _ = fs::remove_file(&path);
    assert!(SOURCE.contains(&format!("rustc-env:{ENABLE_ENV}=1")));
    crate::testing::run_ui(LINT, SOURCE, "", &[]);

    let text = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    let records: Vec<Record> = text
        .lines()
        .map(|line| match serde_json::from_str(line) {
            Ok(Value::Object(map)) => Record(map),
            _ => panic!("not a JSON object: {line}"),
        })
        .collect();
    let header = &records[0];
    assert_eq!(header.str("t"), Some("crate"));
    assert_eq!(header.str("schema"), Some(SCHEMA));
    assert_eq!(header.str("kind"), Some("workspace"));
    assert_eq!(records.last().and_then(|r| r.str("t")), Some("counts"));

    let find = |pred: &dyn Fn(&Record) -> bool| records.iter().find(|r| pred(r));
    let call_to = |caller: &str, callee_name: Option<&str>| {
        find(&|r| {
            r.str("t") == Some("call")
                && r.str("caller").is_some_and(|c| c.ends_with(caller))
                && r.str("callee_name") == callee_name
        })
        .unwrap_or_else(|| panic!("no call {caller} -> {callee_name:?}:\n{text}"))
    };

    let dyn_call = call_to("::dyn_area", Some("area"));
    assert_eq!(dyn_call.str("kind"), Some("dyn"));
    assert_eq!(dyn_call.str("recv"), Some("dyn Shape"));
    let resolved = call_to("::static_area", Some("area"));
    assert_eq!(resolved.str("kind"), Some("trait_static"));
    assert_eq!(resolved.str("resolved_owner"), Some("Square"));
    assert_eq!(
        call_to("::generic_area", Some("area")).str("kind"),
        Some("trait_generic")
    );
    assert_eq!(call_to("::apply", None).str("kind"), Some("fnptr"));
    // The argument of a fn-pointer call is a parameter, not a receiver.
    let fnptr = call_to("::apply_to", None);
    assert_eq!(fnptr.str("kind"), Some("fnptr"));
    assert_eq!(fnptr.str("recv"), None);
    let atomic = call_to("::caller", Some("fetch_add"));
    assert_eq!(
        atomic.0.get("orderings"),
        Some(&serde_json::json!(["Relaxed"]))
    );
    assert!(call_to("::caller", Some("raw")).flag("in_unsafe"));
    assert!(!call_to("::caller", Some("static_area")).flag("in_unsafe"));
    assert_eq!(call_to("::caller", Some("drop")).str("kind"), Some("drop"));
    assert!(
        find(&|r| r.str("t") == Some("ref")
            && r.str("target").is_some_and(|t| t.ends_with("::helper")))
        .is_some()
    );
    assert!(find(&|r| r.str("t") == Some("site") && r.str("kind") == Some("unsafe")).is_some());
    let helper = find(&|r| r.str("t") == Some("item") && r.str("name") == Some("helper"))
        .expect("item record for the macro-defined fn");
    assert_eq!(helper.str("defined_by"), Some("make_helper!"));
    assert!(!helper.flag("gen"), "the name is an argument token");
    let clone_impl = find(&|r| {
        r.str("t") == Some("item")
            && r.str("kind") == Some("impl")
            && r.str("trait_name") == Some("Clone")
    })
    .expect("derived Clone impl");
    assert_eq!(clone_impl.str("defined_by"), Some("#[derive(Clone)]"));
    let coroutine_call = find(&|r| {
        r.str("t") == Some("call")
            && r.str("caller_root").is_some_and(|c| c.ends_with("::ready"))
            && r.str("callee_name") == Some("helper")
    });
    assert!(
        coroutine_call.is_some(),
        "call in the coroutine body:\n{text}"
    );

    let counts = records.last().and_then(|r| r.0.get("counts")).unwrap();
    let count = |key: &str| counts.get(key).and_then(Value::as_u64).unwrap_or(0);
    // `describe` formats an argument: the plumbing is counted, not written.
    assert!(count("fmt_rt_calls_skipped") > 0, "counts: {counts}");
    assert!(!text.contains("core::fmt::rt::"));
    // The initializer of `COUNTER` is const-evaluated during analysis, which
    // consumes its MIR; the coroutine of `ready` has only optimised MIR left.
    assert!(count("mir_ctfe_const") > 0, "counts: {counts}");
    assert!(count("mir_optimized_coroutine") > 0, "counts: {counts}");
}
