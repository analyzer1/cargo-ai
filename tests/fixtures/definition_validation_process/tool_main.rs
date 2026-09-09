use serde_json::{json, Value};

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("describe") => println!(
            "{}",
            json!({
                "protocol_version": 1,
                "name": "envelope_probe",
                "description": "Return controlled protocol envelopes and record consumed values.",
                "params": {
                    "mode": {"type": "string", "required": true},
                    "value": {"type": "string", "required": true}
                },
                "result": {"type": "string", "nullable": true},
                "resource_profile": {
                    "network": "none",
                    "filesystem_read": "none",
                    "filesystem_write": "required",
                    "subprocess": "none",
                    "env_read": "none",
                    "credential_access": "none"
                },
                "self_test": {"supported": false, "safe": false},
                "examples": {
                    "minimal_invoke": {"protocol_version": 1, "params": {"mode": "envelope", "value": "{\"protocol_version\":1,\"result\":null}"}},
                    "full_invoke": {"protocol_version": 1, "params": {"mode": "envelope", "value": "{\"protocol_version\":1,\"result\":\"ok\"}"}}
                }
            })
        ),
        Some("invoke") => {
            let request: Value = serde_json::from_reader(std::io::stdin().lock()).unwrap();
            let params = &request["params"];
            let value = params["value"].as_str().unwrap();
            match params["mode"].as_str().unwrap() {
                "envelope" => println!("{value}"),
                "capture" => {
                    std::fs::write("captured.txt", value).unwrap();
                    println!("{}", json!({"protocol_version": 1, "result": null}));
                }
                "marker" => {
                    std::fs::write("marker.txt", "authorized marker").unwrap();
                    println!("{}", json!({"protocol_version": 1, "result": null}));
                }
                other => panic!("unknown probe mode {other}"),
            }
        }
        other => panic!("unsupported probe command {other:?}"),
    }
}
