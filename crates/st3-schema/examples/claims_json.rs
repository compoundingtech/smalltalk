fn main() {
    let path = std::env::args_os().nth(1).expect("usage: claims_json PATH");
    let mut json = serde_json::to_string_pretty(&st3_schema::registry().claims)
        .expect("serialize claim registry");
    json.push('\n');
    std::fs::write(path, json).expect("write generated convergence corpus");
}
