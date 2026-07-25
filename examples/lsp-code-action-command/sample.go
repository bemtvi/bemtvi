package main

import "fmt"

// Press <leader>ca anywhere in this file. Every action gopls offers here carries a
// `command` and no `edit` — which is the whole point of the example:
//
//   * "Browse gopls feature documentation"  → gopls.client_open_url, handled by the
//     handler in init.lua (only the editor can open a browser).
//   * "Show compiler optimization details"  → gopls.gc_details, not registered, so
//     it round-trips to gopls as workspace/executeCommand.
func Report(values []int) {
	sum := 0
	for _, v := range values {
		sum += v
	}
	fmt.Println("total:", sum)
}

func main() {
	Report([]int{1, 2, 3})
}
