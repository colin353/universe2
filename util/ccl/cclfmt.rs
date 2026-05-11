#[macro_use]
extern crate flags;

use std::io::Read;

fn main() {
    let overwrite = define_flag!("overwrite", false, "whether to overwrite the provided file");
    let args = parse_flags!(overwrite);

    let content = match args.iter().next() {
        Some(f) => std::fs::read_to_string(f).unwrap(),
        None => {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf).unwrap();

            if !buf.ends_with('\n') {
                buf.push('\n');
            }
            buf
        }
    };

    let parsed = ccl::get_ast_or_panic(&content);
    let formatted = ccl::format(parsed, &content);
    if overwrite.value() {
        let path = args
            .iter()
            .next()
            .expect("--overwrite requires a file argument");
        std::fs::write(path, formatted).unwrap();
    } else {
        print!("{}", formatted);
    }
}
