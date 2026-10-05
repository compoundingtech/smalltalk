fn main() {
    let markdown = st3_schema::registry().markdown();
    let mut args = std::env::args_os().skip(1);
    match args.next() {
        Some(option) if option == "--check" => {
            let path = args
                .next()
                .expect("--check requires a generated schema path");
            let existing = std::fs::read_to_string(&path).expect("read generated schema markdown");
            if existing != markdown {
                eprintln!(
                    "schema markdown is stale; run the schema_markdown example with its output path"
                );
                std::process::exit(1);
            }
        }
        Some(path) => std::fs::write(path, markdown).expect("write generated schema markdown"),
        None => print!("{markdown}"),
    }
}
