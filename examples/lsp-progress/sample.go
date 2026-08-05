package main

import (
	"fmt"
	"strings"
)

// Nothing here is remarkable — the example is about the seconds BEFORE gopls can
// answer anything about this file, not about the file. Put the cursor on `Join`
// and press K once the spinner has gone: the hover that arrives instantly then is
// the same request that would have returned nothing while the workspace loaded.
func greet(names []string) string {
	return fmt.Sprintf("hello, %s", strings.Join(names, " and "))
}

func main() {
	fmt.Println(greet([]string{"ada", "grace"}))
}
