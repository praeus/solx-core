//! Test guest for `solx-actions/tests/wasm_nesting.rs`.
//!
//! Three behaviours, selected by the `mode` param:
//!
//! * `nest` — if `depth > 0`, call `self_ref` again with `depth - 1` and
//!   return the nested result's depth. This is the shape that used to pin
//!   one blocking-pool thread per level.
//! * `spin` — loop forever, to exercise epoch preemption and the wall-clock
//!   timeout.
//! * anything else — echo, so a test can measure bare invocation cost.

wit_bindgen::generate!({
    world: "custom-action",
    path: "../../../../solx-wasm/wit",
});

use exports::sol::actions::runner::{ActionResult, Guest};
use sol::actions::action_exec::exec as exec_action;

struct Component;

fn err(msg: impl Into<String>) -> ActionResult {
    ActionResult { success: false, message: Some(msg.into()), output: None }
}

fn ok(output: serde_json::Value) -> ActionResult {
    ActionResult { success: true, message: None, output: Some(output.to_string()) }
}

impl Guest for Component {
    fn run(_action_name: Option<String>, params: String) -> Result<ActionResult, String> {
        let params: serde_json::Value = match serde_json::from_str(&params) {
            Ok(v) => v,
            Err(e) => return Ok(err(format!("bad params: {e}"))),
        };

        let mode = params.get("mode").and_then(|v| v.as_str()).unwrap_or("echo");

        match mode {
            "spin" => {
                // Deliberately unbounded and side-effect-free. Without epoch
                // interruption this pins the worker running the fiber.
                let mut x: u64 = 0;
                loop {
                    x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
                    std::hint::black_box(x);
                }
            }
            "nest" => {
                let depth = params.get("depth").and_then(|v| v.as_u64()).unwrap_or(0);
                if depth == 0 {
                    return Ok(ok(serde_json::json!({ "reached": 0 })));
                }
                let Some(self_ref) = params.get("self_ref").and_then(|v| v.as_str()) else {
                    return Ok(err("nest mode requires 'self_ref'"));
                };
                let next = serde_json::json!({
                    "mode": "nest",
                    "depth": depth - 1,
                    "self_ref": self_ref,
                });
                match exec_action(self_ref, &next.to_string()) {
                    Ok(res) => {
                        if !res.success {
                            return Ok(err(format!(
                                "nested call failed at depth {depth}: {:?}",
                                res.message
                            )));
                        }
                        // Thread the deepest level's marker back up so the
                        // test can assert the whole chain really ran.
                        let inner: serde_json::Value = res
                            .output
                            .as_deref()
                            .and_then(|s| serde_json::from_str(s).ok())
                            .unwrap_or(serde_json::Value::Null);
                        let reached = inner.get("reached").and_then(|v| v.as_u64()).unwrap_or(0);
                        Ok(ok(serde_json::json!({ "reached": reached + 1 })))
                    }
                    Err(e) => Ok(err(format!("exec failed at depth {depth}: {e}"))),
                }
            }
            _ => Ok(ok(serde_json::json!({ "echo": params }))),
        }
    }
}

export!(Component);
