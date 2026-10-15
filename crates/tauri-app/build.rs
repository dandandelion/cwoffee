fn main() {
    tauri_build::build();

    /* Bake the frontend path into the binary.

       The desktop window shows the UI embedded at compile time. The phone and browser
       path reads a dist directory off disk — and the old rule for finding it was
       "walk up from the executable", which pointed at whichever repo the target dir
       happened to live in. Result: the window and the phone showed different builds of
       the UI, and nothing said so. The compile-time answer is the only one that cannot
       go stale, so it is recorded here and preferred at runtime. */
    let manifest = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into()));
    let conf = manifest.join("tauri.conf.json");
    println!("cargo:rerun-if-changed={}", conf.display());
    let raw = match std::fs::read_to_string(&conf) {
        Ok(s) => s,
        Err(_) => return,
    };
    // No JSON parser in a build script for one field. The first quoted string after the
    // key is the value; if the key or its quote shape ever changes, the warning below
    // says so instead of quietly baking nothing.
    let dist = match raw.find("frontendDist").and_then(|i| {
        // After the key comes `": "value"`. Skipping to the first colon and then to the
        // next two quotes is what gets the *value*: taking the quotes either side of the
        // key instead yields `": "`, which canonicalizes to nothing and silently bakes
        // the fallback back in. (It did exactly that on the first attempt.)
        let rest = &raw[i..];
        let c = rest.find(':')? + 1;
        let rest = &rest[c..];
        let a = rest.find('"')? + 1;
        let rest = &rest[a..];
        let b = rest.find('"')?;
        Some(rest[..b].to_string())
    }) {
        Some(d) => d,
        None => {
            eprintln!("cargo:warning=frontendDist not found in tauri.conf.json; the browser path will fall back to looking near the executable");
            return;
        }
    };
    {
        let abs = manifest.join(&dist);
        match std::fs::canonicalize(&abs) {
            Ok(p) => println!("cargo:rustc-env=CWOFFEE_UI_DIST_BAKED={}", p.display()),
            Err(e) => eprintln!(
                "cargo:warning=frontendDist {:?} is not on disk yet ({}): the browser path will fall back to looking near the executable",
                abs, e
            ),
        }
    }
}
