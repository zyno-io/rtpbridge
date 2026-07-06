import { defineConfig } from "playwright/test";

export default defineConfig({
  timeout: 0,
  use: {
    headless: true,
  },
});

