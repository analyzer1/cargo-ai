use serde_json::{Value, json};
use std::io::Write;

fn csv_cell(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("describe") => println!(
            "{}",
            json!({
                "protocol_version": 1,
                "name": "csv_writer",
                "description": "Append structured Animal Patrol observations to project-owned CSV data.",
                "params": {
                    "findings": {"type": "array", "required": true},
                    "location": {"type": "string", "required": true}
                },
                "result": {"type": "string", "nullable": true},
                "resource_profile": {
                    "network": "none", "filesystem_read": "required", "filesystem_write": "required",
                    "subprocess": "none", "env_read": "none", "credential_access": "none"
                },
                "self_test": {"supported": false, "safe": false},
                "examples": {
                    "minimal_invoke": {"protocol_version": 1, "params": {"findings": [], "location": "gate"}},
                    "full_invoke": {"protocol_version": 1, "params": {"findings": [], "location": "gate"}}
                }
            })
        ),
        Some("invoke") => {
            let request: Value = serde_json::from_reader(std::io::stdin().lock()).unwrap();
            let params = &request["params"];
            let findings = params["findings"].as_array().unwrap();
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open("findings.csv")
                .unwrap();
            if file.metadata().unwrap().len() == 0 {
                writeln!(file, "revision,location,animal,count,confidence,note").unwrap();
            }
            for finding in findings {
                let fields = [
                    "1".to_string(),
                    params["location"].as_str().unwrap().to_string(),
                    finding["animal"].as_str().unwrap().to_string(),
                    finding["count"].as_i64().unwrap().to_string(),
                    finding["confidence"].as_f64().unwrap().to_string(),
                    finding["note"].as_str().unwrap_or("").to_string(),
                ];
                writeln!(
                    file,
                    "{}",
                    fields
                        .iter()
                        .map(|field| csv_cell(field))
                        .collect::<Vec<_>>()
                        .join(",")
                )
                .unwrap();
            }
            println!(
                "{}",
                json!({"protocol_version": 1, "result": format!("Recorded {} findings", findings.len())})
            );
        }
        other => panic!("unsupported CSV writer command: {other:?}"),
    }
}
