#!/usr/bin/env bash
foo="bar baz"
# Deliberate issue: an unquoted expansion, which the bash language server (via
# shellcheck) reports as SC2086 ("Double quote to prevent globbing and word
# splitting"). Requires `shellcheck` on PATH.
echo $foo
