;; extends

; Closures as function objects. The Rust query `:TSInstall rust` fetches (from
; nvim-treesitter-textobjects) only captures a closure's parameters, so without this
; `vif` inside a closure selects the whole enclosing `fn`. With it, `vif` takes the
; closure body and `v2if` the function around it, as init.lua says. `@function.inner`
; is the inside of a block body (`|v| { … }`); an expression body has no braces to
; select inside, so there `vif` falls back to the whole closure.
(closure_expression) @function.outer

(closure_expression
  body: (block
    .
    "{"
    _+ @function.inner
    "}"))
