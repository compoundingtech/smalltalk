fn main() {
    let corpus = serde_json::to_string_pretty(&st3_schema::registry().claims)
        .expect("serialize convergence claim corpus");
    match std::env::args_os().nth(1) {
        Some(path) => std::fs::write(path, corpus).expect("write convergence claim corpus"),
        None => print!("{corpus}"),
    }
}
