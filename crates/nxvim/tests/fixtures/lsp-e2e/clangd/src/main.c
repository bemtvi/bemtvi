int main(void) {
    /* Deliberate error: `undeclared_variable` is never declared, so clangd must
       report a "use of undeclared identifier" diagnostic on this line. */
    return undeclared_variable;
}
