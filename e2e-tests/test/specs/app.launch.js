// OxideLink E2E smoke test — verifies the app window launches and shows the main title.
describe("OxideLink app launch", () => {
  it("should open the main window with the correct title", async () => {
    const title = await browser.getTitle();
    // The window title is set in tauri.conf.json → app.windows[0].title
    expect(title).toBe("OxideLink");
  });

  it("should show the connection chip in the telemetry panel", async () => {
    const chip = await $("#connection-chip");
    await chip.waitForDisplayed({ timeout: 15000 });
    expect(await chip.isDisplayed()).toBe(true);
  });

  it("should show the battery panel", async () => {
    const fill = await $("#battery-fill");
    await fill.waitForExist({ timeout: 10000 });
    expect(await fill.isExisting()).toBe(true);
  });
});
