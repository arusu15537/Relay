//! Outlines of read-only context files for the local model.
//!
//! An outline is the file with the bodies of functions and methods (and the interior of long
//! literals such as big strings, arrays and dicts) replaced by one marker line, for example
//! `    # … (12 lines omitted)`. Every other line is copied verbatim, so imports, constants, type
//! definitions, signatures, decorators, doc comments and docstrings appear exactly as in the
//! file, at a fraction of the tokens.
//!
//! Parsing uses tree-sitter. Languages without a grammar here have no outline; callers send
//! those files in full.

use tree_sitter::{Language, Node, Parser};

/// Bodies with fewer lines than this are kept: the marker would save almost nothing.
pub const MIN_BODY_LINES: usize = 3;
/// Literals spanning at least this many lines are collapsed to their first and last line.
pub const LONG_LITERAL_LINES: usize = 12;
/// Larger files are not parsed (they would not fit any context window anyway).
const MAX_BYTES: usize = 4 * 1024 * 1024;

/// Nodes whose `body` field is elided, across all supported grammars.
const FUNCTION_KINDS: &[&str] = &[
    // Python, C, C++
    "function_definition",
    // JavaScript, TypeScript, Go
    "function_declaration",
    "function_expression",
    "generator_function",
    "generator_function_declaration",
    "arrow_function",
    "method_definition",
    "class_static_block",
    "func_literal",
    // Rust
    "function_item",
    "closure_expression",
    // Go, Java, C#
    "method_declaration",
    "constructor_declaration",
    "compact_constructor_declaration",
    "destructor_declaration",
    "operator_declaration",
    "conversion_operator_declaration",
    "local_function_statement",
    "accessor_declaration",
    // Java, C#, C++
    "lambda_expression",
];

/// Literal nodes that are collapsed when they span at least [`LONG_LITERAL_LINES`] lines and
/// contain no functions.
const LITERAL_KINDS: &[&str] = &[
    "string",
    "concatenated_string",
    "template_string",
    "string_literal",
    "raw_string_literal",
    "interpreted_string_literal",
    "verbatim_string_literal",
    "dictionary",
    "list",
    "set",
    "array",
    "object",
    "array_expression",
    "composite_literal",
    "array_initializer",
    "initializer_list",
    "initializer_expression",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Style {
    /// Bodies are delimited by their first and last line (`{ … }`): keep both, drop the inside.
    Braces,
    /// Python: the body is the indented block after the `def …:` line.
    Indent,
}

struct Lang {
    language: Language,
    style: Style,
    comment: &'static str,
}

fn lang_for(path: &str) -> Option<Lang> {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    let (language, style, comment): (Language, Style, &'static str) = match ext.as_str() {
        "py" | "pyi" => (tree_sitter_python::LANGUAGE.into(), Style::Indent, "#"),
        "js" | "jsx" | "mjs" | "cjs" => (tree_sitter_javascript::LANGUAGE.into(), Style::Braces, "//"),
        "ts" | "mts" | "cts" => (tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(), Style::Braces, "//"),
        "tsx" => (tree_sitter_typescript::LANGUAGE_TSX.into(), Style::Braces, "//"),
        "rs" => (tree_sitter_rust::LANGUAGE.into(), Style::Braces, "//"),
        "go" => (tree_sitter_go::LANGUAGE.into(), Style::Braces, "//"),
        "java" => (tree_sitter_java::LANGUAGE.into(), Style::Braces, "//"),
        "cs" => (tree_sitter_c_sharp::LANGUAGE.into(), Style::Braces, "//"),
        "c" => (tree_sitter_c::LANGUAGE.into(), Style::Braces, "//"),
        // `.h` is often C++; the C++ grammar also parses plain C headers.
        "h" | "hh" | "hpp" | "hxx" | "cc" | "cpp" | "cxx" => (tree_sitter_cpp::LANGUAGE.into(), Style::Braces, "//"),
        _ => return None,
    };
    Some(Lang {
        language,
        style,
        comment,
    })
}

/// True if [`outline`] knows the file's language.
pub fn supports(path: &str) -> bool {
    lang_for(path).is_some()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outline {
    pub text: String,
    /// Source lines replaced by markers.
    pub omitted_lines: usize,
}

/// Last row a node covers (a node ending at column 0 ends with the previous line's newline).
fn last_row(n: Node) -> usize {
    let s = n.start_position();
    let e = n.end_position();
    if e.column == 0 && e.row > s.row { e.row - 1 } else { e.row }
}

/// Rows strictly between a node's first and last row, if there are enough of them.
fn interior(n: Node) -> Option<(usize, usize)> {
    let first = n.start_position().row + 1;
    let last = last_row(n).checked_sub(1)?;
    (last + 1 >= first + MIN_BODY_LINES).then_some((first, last))
}

/// Statements that can define a function in their first line (`const send = (m) => …`).
const DECLARATION_KINDS: &[&str] = &[
    "lexical_declaration",
    "variable_declaration",
    "let_declaration",
    "short_var_declaration",
    "local_declaration_statement",
    "local_variable_declaration",
    "expression_statement",
    "decorated_definition",
];

/// True if a body defines functions at statement level (a closure factory such as
/// `function connect() { const send = …; return { send }; }` or a decorator factory).
fn defines_inner_functions(body: Node) -> bool {
    let mut cursor = body.walk();
    body.named_children(&mut cursor).any(|c| {
        if FUNCTION_KINDS.contains(&c.kind()) {
            return true;
        }
        if !DECLARATION_KINDS.contains(&c.kind()) {
            return false;
        }
        let row = c.start_position().row;
        let mut stack = vec![c];
        while let Some(x) = stack.pop() {
            if x.start_position().row != row {
                continue;
            }
            if FUNCTION_KINDS.contains(&x.kind()) {
                return true;
            }
            let mut cur = x.walk();
            stack.extend(x.named_children(&mut cur));
        }
        false
    })
}

/// In a closure factory, the final `return { … }` names the API the factory hands out: keep it
/// (when it is at most two lines) by ending the omitted range before it.
fn keep_factory_return(body: Node, range: (usize, usize)) -> Option<(usize, usize)> {
    if !defines_inner_functions(body) {
        return Some(range);
    }
    let mut cursor = body.walk();
    let last_stmt = body.named_children(&mut cursor).filter(|c| c.kind() != "comment").last();
    match last_stmt {
        Some(r) if r.kind() == "return_statement" && last_row(r) <= r.start_position().row + 1 => {
            let end = r.start_position().row.checked_sub(1)?;
            (end + 1 >= range.0 + MIN_BODY_LINES).then_some((range.0, end))
        }
        _ => Some(range),
    }
}

/// The rows of a function's body to omit.
fn body_range(func: Node, style: Style) -> Option<(usize, usize)> {
    if func.has_error() {
        return None;
    }
    let body = func.child_by_field_name("body")?;
    match style {
        Style::Braces => interior(body).and_then(|r| keep_factory_return(body, r)),
        Style::Indent => {
            // `def f(): return 1` keeps its body on the header line.
            if body.prev_sibling().is_some_and(|colon| colon.end_position().row == body.start_position().row) {
                return None;
            }
            let mut first = body.start_position().row;
            let mut cursor = body.walk();
            let docstring = body
                .named_children(&mut cursor)
                .find(|c| c.kind() != "comment")
                .filter(|c| c.kind() == "expression_statement")
                .and_then(|c| c.named_child(0))
                .filter(|s| s.kind() == "string" || s.kind() == "concatenated_string");
            if let Some(d) = docstring {
                first = last_row(d) + 1;
            }
            let last = last_row(body);
            (last + 1 >= first + MIN_BODY_LINES)
                .then_some((first, last))
                .and_then(|r| keep_factory_return(body, r))
        }
    }
}

fn contains_function(n: Node) -> bool {
    let mut stack = vec![n];
    while let Some(x) = stack.pop() {
        if FUNCTION_KINDS.contains(&x.kind()) {
            return true;
        }
        let mut cursor = x.walk();
        stack.extend(x.named_children(&mut cursor));
    }
    false
}

/// Python docstrings (a string that is a statement of its own) are kept even when long.
fn is_docstring(n: Node, style: Style) -> bool {
    style == Style::Indent && n.parent().is_some_and(|p| p.kind() == "expression_statement")
}

fn leading_ws(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

/// The outline of `content`, or `None` if the language is not supported, the file is too large,
/// or nothing would be omitted.
pub fn outline(path: &str, content: &str) -> Option<Outline> {
    if content.len() > MAX_BYTES {
        return None;
    }
    let lang = lang_for(path)?;
    let mut parser = Parser::new();
    parser.set_language(&lang.language).ok()?;
    let tree = parser.parse(content, None)?;

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if FUNCTION_KINDS.contains(&kind) {
            // Never descend into a function: a kept (short) body has nothing worth eliding.
            if let Some(r) = body_range(node, lang.style) {
                ranges.push(r);
            }
            continue;
        }
        if LITERAL_KINDS.contains(&kind)
            && !node.has_error()
            && last_row(node) + 1 - node.start_position().row >= LONG_LITERAL_LINES
            && !is_docstring(node, lang.style)
            && !contains_function(node)
        {
            if let Some(r) = interior(node) {
                ranges.push(r);
            }
            continue;
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    if ranges.is_empty() {
        return None;
    }
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (a, b) in ranges {
        match merged.last_mut() {
            Some(last) if a <= last.1 + 1 => last.1 = last.1.max(b),
            _ => merged.push((a, b)),
        }
    }

    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    let mut out = String::with_capacity(content.len() / 2);
    let mut omitted = 0;
    let mut i = 0;
    for (a, b) in merged {
        if a >= lines.len() {
            break;
        }
        let b = b.min(lines.len() - 1);
        while i < a {
            out.push_str(lines[i]);
            i += 1;
        }
        let indent = lines[a..=b]
            .iter()
            .find(|l| !l.trim().is_empty())
            .map(|l| leading_ws(l))
            .unwrap_or("");
        let n = b - a + 1;
        out.push_str(&format!("{indent}{} … ({n} lines omitted)\n", lang.comment));
        omitted += n;
        i = b + 1;
    }
    for l in &lines[i.min(lines.len())..] {
        out.push_str(l);
    }
    (omitted > 0).then_some(Outline {
        text: out,
        omitted_lines: omitted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every line of the outline is either a marker or a verbatim line of the file, in order.
    fn assert_verbatim(original: &str, o: &Outline) {
        let mut orig = original.lines();
        for line in o.text.lines() {
            if line.trim_start().contains("… (") && line.trim_end().ends_with("lines omitted)") {
                continue;
            }
            assert!(orig.any(|l| l == line), "line not in the original (or out of order): {line:?}\n{}", o.text);
        }
    }

    const PY: &str = r#""""Module docstring."""
import os
from typing import Optional

TIMEOUT = 10


def short(x):
    return x + 1


@decorator
def long_function(a: int, b: Optional[str] = None) -> str:
    """Explain what it does.

    More detail.
    """
    if a:
        return "a"
    elif b:
        return b
    return "none"


class Client:
    """A client."""

    retries = 3

    def __init__(self, url):
        self.url = url
        self.session = None
        self.closed = False

    def one_liner(self): return 1

    def multi_line_sig(
        self,
        a,
        b,
    ):
        x = a + b
        y = x * 2
        return y
"#;

    #[test]
    fn python_keeps_signatures_docstrings_and_short_bodies() {
        let o = outline("pkg/client.py", PY).expect("outline");
        assert_verbatim(PY, &o);
        let t = &o.text;
        for kept in [
            "\"\"\"Module docstring.\"\"\"",
            "import os",
            "TIMEOUT = 10",
            "def short(x):\n    return x + 1\n",
            "@decorator\ndef long_function(a: int, b: Optional[str] = None) -> str:\n    \"\"\"Explain what it does.\n\n    More detail.\n    \"\"\"\n    # … (5 lines omitted)\n",
            "class Client:\n    \"\"\"A client.\"\"\"\n\n    retries = 3\n",
            "    def __init__(self, url):\n        # … (3 lines omitted)\n",
            "    def one_liner(self): return 1\n",
            "    def multi_line_sig(\n        self,\n        a,\n        b,\n    ):\n        # … (3 lines omitted)\n",
        ] {
            assert!(t.contains(kept), "missing {kept:?} in\n{t}");
        }
        assert!(!t.contains("return \"none\""));
        assert_eq!(o.omitted_lines, 11);
    }

    const RS: &str = r##"use std::fmt;

/// A point.
pub struct Point {
    pub x: i32,
    pub y: i32,
}

const TEMPLATE: &str = r#"line 1
line 2
line 3
line 4
line 5
line 6
line 7
line 8
line 9
line 10
line 11
line 12
"#;

impl Point {
    /// Distance from the origin.
    pub fn norm(&self) -> f64 {
        let x = self.x as f64;
        let y = self.y as f64;
        (x * x + y * y).sqrt()
    }

    pub fn tiny(&self) -> i32 { self.x }
}

pub trait Shape {
    fn area(&self) -> f64;
}
"##;

    #[test]
    fn rust_elides_fn_bodies_and_long_literals() {
        let o = outline("src/geo.rs", RS).expect("outline");
        assert_verbatim(RS, &o);
        let t = &o.text;
        assert!(t.contains("pub struct Point {\n    pub x: i32,\n    pub y: i32,\n}"), "{t}");
        assert!(t.contains("    /// Distance from the origin.\n    pub fn norm(&self) -> f64 {\n        // … (3 lines omitted)\n    }\n"), "{t}");
        assert!(t.contains("pub fn tiny(&self) -> i32 { self.x }"));
        assert!(t.contains("fn area(&self) -> f64;"));
        assert!(t.contains("const TEMPLATE: &str = r#\"line 1\n// … (11 lines omitted)\n\"#;"), "{t}");
    }

    #[test]
    fn typescript_and_tsx_elide_functions_methods_and_arrows() {
        let ts = "import { a } from './a';\n\nexport interface Opts {\n  retries: number;\n  delay: number;\n}\n\nexport class Api {\n  constructor(private url: string) {}\n\n  async get(path: string): Promise<string> {\n    const r = await fetch(this.url + path);\n    if (!r.ok) throw new Error('x');\n    return r.text();\n  }\n}\n\nexport const helper = (x: number): number => {\n  const y = x * 2;\n  const z = y + 1;\n  return z;\n};\n";
        let o = outline("src/api.ts", ts).expect("outline");
        assert_verbatim(ts, &o);
        assert!(o.text.contains("export interface Opts {\n  retries: number;\n  delay: number;\n}"));
        assert!(o.text.contains("  async get(path: string): Promise<string> {\n    // … (3 lines omitted)\n  }"), "{}", o.text);
        assert!(o.text.contains("export const helper = (x: number): number => {\n  // … (3 lines omitted)\n};"), "{}", o.text);
        assert!(o.text.contains("constructor(private url: string) {}"));

        let tsx = "export function App({ name }: { name: string }) {\n  const [n, setN] = useState(0);\n  useEffect(() => {}, []);\n  const label = name.trim();\n  return (\n    <div>{label}</div>\n  );\n}\n";
        let o = outline("src/App.tsx", tsx).expect("outline");
        assert!(o.text.contains("export function App({ name }: { name: string }) {\n  // … (6 lines omitted)\n}"), "{}", o.text);
    }

    #[test]
    fn closure_factories_keep_their_final_return() {
        let js = "export async function connect() {\n  let page;\n  const ws = open(page);\n  const send = (method, params = {}) =>\n    new Promise((res) => {\n      ws.send(method, params, res);\n    });\n  const click = async (text) => {\n    await send('click', { text });\n  };\n  return { ws, send, click };\n}\n";
        let o = outline("cdp.mjs", js).expect("outline");
        assert_verbatim(js, &o);
        assert_eq!(o.text, "export async function connect() {\n  // … (9 lines omitted)\n  return { ws, send, click };\n}\n");

        let py = "def retry(times):\n    \"\"\"Decorator factory.\"\"\"\n    def deco(fn):\n        def wrapper(*a):\n            return fn(*a)\n        return wrapper\n    return deco\n";
        let o = outline("r.py", py).expect("outline");
        assert_eq!(o.text, "def retry(times):\n    \"\"\"Decorator factory.\"\"\"\n    # … (4 lines omitted)\n    return deco\n");

        // A long final return (typical JSX) is not kept; neither is the return of a plain function.
        let plain = "function f(x) {\n  const a = x + 1;\n  const b = a * 2;\n  const c = b - 3;\n  return c;\n}\n";
        assert_eq!(outline("p.js", plain).unwrap().text, "function f(x) {\n  // … (4 lines omitted)\n}\n");
    }

    #[test]
    fn javascript_object_with_methods_keeps_method_signatures() {
        let js = "module.exports = {\n  name: 'x',\n  run(a, b) {\n    const c = a + b;\n    console.log(c);\n    return c;\n  },\n  a: 1,\n  b: 2,\n  c: 3,\n  d: 4,\n  e: 5,\n  f: 6,\n  g: 7,\n};\n";
        let o = outline("index.js", js).expect("outline");
        assert_verbatim(js, &o);
        assert!(o.text.contains("  run(a, b) {\n    // … (3 lines omitted)\n  },\n  a: 1,"), "{}", o.text);
    }

    #[test]
    fn every_grammar_loads_and_elides_a_function() {
        let three = "a();\n    b();\n    c();";
        let cases = [
            ("m.go", format!("package m\n\nfunc F(x int) int {{\n    {three}\n    return x\n}}\n")),
            ("M.java", format!("class M {{\n  int f(int x) {{\n    {three}\n    return x;\n  }}\n}}\n")),
            ("M.cs", format!("class M {{\n  int F(int x) {{\n    {three}\n    return x;\n  }}\n}}\n")),
            ("m.c", format!("int f(int x) {{\n    {three}\n    return x;\n}}\n")),
            ("m.cpp", format!("namespace n {{\nint f(int x) {{\n    {three}\n    return x;\n}}\n}}\n")),
            ("m.h", format!("class M {{\n public:\n  int f(int x) {{\n    {three}\n    return x;\n  }}\n}};\n")),
            ("m.jsx", format!("function f(x) {{\n    {three}\n    return x;\n}}\n")),
            ("m.mjs", format!("export function f(x) {{\n    {three}\n    return x;\n}}\n")),
        ];
        for (path, src) in cases {
            let o = outline(path, &src).unwrap_or_else(|| panic!("{path}: no outline"));
            assert!(o.text.contains("// … (4 lines omitted)"), "{path}:\n{}", o.text);
            assert_verbatim(&src, &o);
        }
    }

    #[test]
    fn unsupported_or_nothing_to_omit_gives_none() {
        assert!(outline("README.md", "# hi\n").is_none());
        assert!(outline("data.json", "{}").is_none());
        assert!(outline("Makefile", "all:\n\techo\n").is_none());
        assert!(!supports("x.kt") && supports("x.PY") && supports("dir.v2/x.rs"));
        assert!(outline("a.py", "def f():\n    return 1\n").is_none(), "short bodies are kept");
    }

    #[test]
    fn crlf_and_broken_code_are_handled() {
        let crlf = "def f():\r\n    a = 1\r\n    b = 2\r\n    return a + b\r\n";
        let o = outline("a.py", crlf).expect("outline");
        assert_eq!(o.text, "def f():\r\n    # … (3 lines omitted)\n");

        // A syntax error inside one function leaves that function alone but still outlines others.
        let broken = "def ok():\n    a = 1\n    b = 2\n    return a\n\ndef bad(:\n    x = (\n    y = 2\n    return y\n";
        if let Some(o) = outline("b.py", broken) {
            assert_verbatim(broken, &o);
            assert!(o.text.contains("def bad(:"));
        }
    }
}
