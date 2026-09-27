fn main() {
    let markdown = st3_schema::registry().markdown();
    match std::env::args_os().nth(1) {
        Some(path) => std::fs::write(path, markdown).expect("write generated schema markdown"),
        None => print!("{markdown}"),
    }
}
