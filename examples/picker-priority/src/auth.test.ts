import { useAuth } from "./auth"

test("useAuth starts signed out", () => {
  expect(useAuth().user).toBe(null)
})
