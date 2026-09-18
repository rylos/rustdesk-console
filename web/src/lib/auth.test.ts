import { describe, expect, it } from "vitest";
import { currentUserQueryKey } from "./auth";

describe("current user cache", () => {
  it("separates user data by auth session without using the token", () => {
    expect(currentUserQueryKey("standard-session")).not.toEqual(
      currentUserQueryKey("admin-session"),
    );
  });
});
