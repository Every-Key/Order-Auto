import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { App } from "./App";

describe("App", () => {
  it("identifies the desktop application", () => {
    render(<App />);

    expect(screen.getByRole("heading", { name: "OrderPilot" })).toBeVisible();
  });
});
