# Clone Discipline

## Directive

Detect and challenge redundant and/or excessive uses of `.clone()`.

## Definition

The "Attack of the Clones" anti-pattern in Rust refers to the excessive and often unnecessary use of the
`.clone()` method to bypass ownership and lifetime challenges. It occurs when developers choose to create
deep copies of data as a "path of least resistance" to satisfy the borrow checker, rather than
addressing the underlying structural or ownership issues in the code.

## Anti Pattern Examples

### BAD: Cloning a `Struct` when a `&Struct` will do

```rs
// BAD: prefer using `line: &str` which eliminates the .clone() below
fn make_lipographic(banned: char, line: String) -> String {
    line.as_str().chars().filter(|&c| c != banned).collect()
}

fn main() {
    let passage = String::from("If Youth, throughout all history, had had a champion to stand up for it");
    assert_eq!(make_lipographic('e', passage.clone()), passage);
}
```

### BAD: Cloning a `Vec` when iterating

```rs
fn main() {
    let mut bad_letters = vec!['e', 't', 'o', 'i'];
    // BAD: prefer `for l in &bad_letters {`
    for l in bad_letters.clone() {
        // do something here
    }
    bad_letters.push('s');
}
```

### BAD: Cloning when handling an `Option` or `Result`

```rs
pub struct LipogramCorpora {
    selections: Vec<(char, Option<String>)>,
}

impl LipogramCorpora {
    pub fn validate_all(&mut self) -> Result<(), char> {
        for selection in &self.selections {
            if selection.1.is_some() {
                // BAD. prefer `if let Some(s) = selection.1.as_deref()` or pattern matching
                if selection.1.clone().unwrap().contains(selection.0) {
                    return Err(selection.0);
                }
            }
        }
        Ok(())
    }
}
```

### BAD: Cloning when passing immutable Struct into a closure

If a `move` or `async move` closure needs immutable access to a Struct
that is expensive to clone, it's good practice to share it via `Arc<_>`
instead of `.clone()`.

```rs
// BAD
let big_chunk = BigChunk::new();

for _ in 1..10 {
    let big_chunk = big_chunk.clone();
    tokio::spawn(async move {
        do_something_immutable(&big_chunk);
    });
}
```

```rs
// GOOD
let big_chunk = Arc::new(BigChunk::new());

for _ in 1..10 {
    let big_chunk = Arc::clone(&big_chunk);
    tokio::spawn(async move {
        do_something_immutable(&big_chunk);
    });
}
```

## Limitation

Ignore redundant clones if their elimination would require adding explicit lifetime
annotations (`'a`).

<example>
Using `.clone()` (Simple)
The Config struct is "self-contained" and easy to pass around.

```rs
struct Config {
    path: String,
}
fn load_config(path: &str) -> Config {
    // Cloning ensures the Config owns its data
    // and doesn't depend on the 'path' reference.
    Config {
        path: path.to_string(),
    }
}
```

Eliminating `.clone()` (Complex)
To remove the clone, we must introduce a lifetime. This is "complex" because the 'a now must be
propagated to every single place Config is used, potentially requiring dozens of changes across
the codebase.

```rs
// Now the struct is bound to a lifetime
struct Config<'a> {
    path: &'a str,
}
// Every function returning or holding Config must now
// manage these annotations.
fn load_config<'a>(path: &'a str) -> Config<'a> {
    Config {
           path,
    }
}

// Any struct containing Config also becomes complex:
struct App<'a> {
    config: Config<'a>,
}
```
</example>
