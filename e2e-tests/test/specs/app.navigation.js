// OxideLink E2E navigation tests — verifies tab switching and panel visibility.
describe("OxideLink tab navigation", () => {
  it("should show the Controller tab as active by default", async () => {
    const controllerTab = await $("#tab-controller");
    await controllerTab.waitForDisplayed({ timeout: 15000 });
    expect(await controllerTab.isDisplayed()).toBe(true);

    const tabBtn = await $('.tab-btn[data-tab="controller"]');
    expect(await tabBtn.getAttribute("class")).toContain("active");
  });

  it("should switch to the IMU & Motion tab when clicked", async () => {
    const tabBtn = await $('.tab-btn[data-tab="imu"]');
    await tabBtn.click();

    const imuPanel = await $("#tab-imu");
    await imuPanel.waitForDisplayed({ timeout: 5000 });
    expect(await imuPanel.isDisplayed()).toBe(true);

    // Controller tab should no longer be active
    const controllerTab = await $("#tab-controller");
    expect(await controllerTab.isDisplayed()).toBe(false);
  });

  it("should switch to the Lights tab when clicked", async () => {
    const tabBtn = await $('.tab-btn[data-tab="lights"]');
    await tabBtn.click();

    const lightsPanel = await $("#tab-lights");
    await lightsPanel.waitForDisplayed({ timeout: 5000 });
    expect(await lightsPanel.isDisplayed()).toBe(true);
  });

  it("should switch to the Calibration tab when clicked", async () => {
    const tabBtn = await $('.tab-btn[data-tab="calibration"]');
    await tabBtn.click();

    const calPanel = await $("#tab-calibration");
    await calPanel.waitForDisplayed({ timeout: 5000 });
    expect(await calPanel.isDisplayed()).toBe(true);
  });

  it("should switch to the Logging tab when clicked", async () => {
    const tabBtn = await $('.tab-btn[data-tab="logging"]');
    await tabBtn.click();

    const loggingPanel = await $("#tab-logging");
    await loggingPanel.waitForDisplayed({ timeout: 5000 });
    expect(await loggingPanel.isDisplayed()).toBe(true);
  });

  it("should switch to the Profiles tab when clicked", async () => {
    const tabBtn = await $('.tab-btn[data-tab="profiles"]');
    await tabBtn.click();

    const profilesPanel = await $("#tab-profiles");
    await profilesPanel.waitForDisplayed({ timeout: 5000 });
    expect(await profilesPanel.isDisplayed()).toBe(true);
  });

  it("should switch to the Settings tab when clicked", async () => {
    const tabBtn = await $('.tab-btn[data-tab="settings"]');
    await tabBtn.click();

    const settingsPanel = await $("#tab-settings");
    await settingsPanel.waitForDisplayed({ timeout: 5000 });
    expect(await settingsPanel.isDisplayed()).toBe(true);
  });

  it("should switch to the Cloud tab when clicked", async () => {
    const tabBtn = await $('.tab-btn[data-tab="cloud"]');
    await tabBtn.click();

    const cloudPanel = await $("#tab-cloud");
    await cloudPanel.waitForDisplayed({ timeout: 5000 });
    expect(await cloudPanel.isDisplayed()).toBe(true);
  });
});
