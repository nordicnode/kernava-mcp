use criterion::{black_box, criterion_group, criterion_main, Criterion};
use kernava_indexer::{builder, parser};
use kernava_store::Store;

fn bench_parse(c: &mut Criterion) {
    let ts = "function add(a: number, b: number): number { return a + b; }";
    c.bench_function("parse_ts", |b| {
        b.iter(|| parser::parse(black_box(ts), parser::Language::TypeScript).unwrap());
    });

    let py = "def add(a, b):\n    return a + b\n";
    c.bench_function("parse_python", |b| {
        b.iter(|| parser::parse(black_box(py), parser::Language::Python).unwrap());
    });

    let rs = "fn add(a: i32, b: i32) -> i32 { a + b }";
    c.bench_function("parse_rust", |b| {
        b.iter(|| parser::parse(black_box(rs), parser::Language::Rust).unwrap());
    });
}

fn bench_index_file(c: &mut Criterion) {
    let dir = std::env::temp_dir().join("kernava_bench");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("main.ts"),
        "import { add } from './math';\nfunction main() { return add(1, 2); }\n",
    )
    .unwrap();
    std::fs::write(dir.join("math.ts"), "export function add(a, b) { return a + b; }\n").unwrap();

    c.bench_function("index_file_ts", |b| {
        b.iter(|| {
            let mut store = Store::open_in_memory().unwrap();
            builder::index_file(&mut store, &dir.join("main.ts")).unwrap();
        });
    });

    let _ = std::fs::remove_dir_all(&dir);
}

fn bench_index_full(c: &mut Criterion) {
    let dir = std::env::temp_dir().join("kernava_bench_full");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for i in 0..10 {
        let name = format!("mod{i}.ts");
        let src = format!("export function fn{i}() {{ return {i}; }}\n");
        std::fs::write(dir.join(&name), src).unwrap();
    }
    std::fs::write(
        dir.join("main.ts"),
        "import { fn0 } from './mod0';\nfunction main() { return fn0(); }\n",
    )
    .unwrap();

    c.bench_function("index_full_11_files", |b| {
        b.iter(|| {
            let mut store = Store::open_in_memory().unwrap();
            builder::index_full(&mut store, &dir).unwrap();
        });
    });

    let _ = std::fs::remove_dir_all(&dir);
}

fn bench_query(c: &mut Criterion) {
    let dir = std::env::temp_dir().join("kernava_bench_query");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("app.ts"), "export function handleRequest() { return 42; }\n").unwrap();

    let mut store = Store::open_in_memory().unwrap();
    builder::index_full(&mut store, &dir).unwrap();

    c.bench_function("search_symbols_fts5", |b| {
        b.iter(|| {
            kernava_store::fts5::search_symbols(store.conn(), black_box("handle"), 10).unwrap();
        });
    });

    c.bench_function("search_symbols_cross_style", |b| {
        b.iter(|| {
            kernava_store::fts5::search_symbols(store.conn(), black_box("handle_request"), 10)
                .unwrap();
        });
    });

    let _ = std::fs::remove_dir_all(&dir);
}

criterion_group!(benches, bench_parse, bench_index_file, bench_index_full, bench_query);
criterion_main!(benches);
