# Markdown rendering

Press `K` (or run `:MarkdownFloat`) to render **this buffer** into a styled popup —
the markup is *rendered*, not shown verbatim.

## Inline styles

You get **bold**, *italic*, ~~strikethrough~~, `inline code`, and
[links](https://bemtvi.example/docs) that keep their label and show the URL.

## Lists

- a bullet becomes a real glyph
- with **styled** text inside
  - and nested items indent

1. ordered lists
2. keep their numbers

## Task lists

- [x] rendered as a checkbox
- [ ] not a literal `[ ]`

## Block quotes

> A quote gets a styled bar down its left edge,
> across every line it spans.

## Tables

| Feature   | Rendered |
|-----------|----------|
| headings  | yes      |
| tables    | aligned  |

## Code blocks

```rust
fn main() {
    println!("fenced code keeps its language highlighting");
}
```

---

Everything above is one call to `btv.markdown.render`.
