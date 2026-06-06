export function greet(name: string): string {
  return "Hello, " + name;
}

// Deliberate error: a string is not assignable to `number`, so the TypeScript
// server must report a TS2322 diagnostic on this line.
const count: number = "not a number";

console.log(greet("world"), count);
