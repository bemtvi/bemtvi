/* A tiny sample "C" file with deliberate mistakes for the quickfix tour.
 * The fake compiler (fakecc.sh) and `:grep TODO` below point their hits at the
 * line numbers called out here, so :make / :grep / :vimgrep all land somewhere
 * real when you step through the list with :cnext / <CR>.
 */
#include <stdio.h>

int add(int a, int b) {
    return a + b   /* line 9: missing semicolon */
}

int main(void) {
    int total = add(2, 3);
    printf("total = %d\n", totl);   /* line 14: typo'd `totl` (TODO: fix) */
    undeclared_helper();            /* line 15: unknown function (TODO) */
    return 0;
}
