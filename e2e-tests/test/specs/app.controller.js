// OxideLink E2E controller panel tests — verifies wireframe and button elements.
describe("OxideLink controller panel", () => {
  it("should display the controller wireframe SVG", async () => {
    const wireframe = await $("#panel-wireframe");
    await wireframe.waitForDisplayed({ timeout: 15000 });
    expect(await wireframe.isDisplayed()).toBe(true);
  });

  it("should show all face buttons in the wireframe", async () => {
    const btnA = await $("#btn-a");
    const btnB = await $("#btn-b");
    const btnX = await $("#btn-x");
    const btnY = await $("#btn-y");

    for (const btn of [btnA, btnB, btnX, btnY]) {
      expect(await btn.isExisting()).toBe(true);
    }
  });

  it("should show shoulder and trigger buttons", async () => {
    const btnL = await $("#btn-l");
    const btnR = await $("#btn-r");
    const btnZL = await $("#btn-zl");
    const btnZR = await $("#btn-zr");

    for (const btn of [btnL, btnR, btnZL, btnZR]) {
      expect(await btn.isExisting()).toBe(true);
    }
  });

  it("should show center buttons (minus, plus, home, capture)", async () => {
    const btnMinus = await $("#btn-minus");
    const btnPlus = await $("#btn-plus");
    const btnHome = await $("#btn-home");
    const btnCapture = await $("#btn-capture");

    for (const btn of [btnMinus, btnPlus, btnHome, btnCapture]) {
      expect(await btn.isExisting()).toBe(true);
    }
  });

  it("should show the keep-alive panel with boost button", async () => {
    const kaPanel = await $("#panel-keepalive");
    expect(await kaPanel.isExisting()).toBe(true);

    const boostBtn = await $("#btn-boost");
    expect(await boostBtn.isExisting()).toBe(true);
  });

  it("should show the remap panel", async () => {
    const remapPanel = await $("#panel-remap");
    expect(await remapPanel.isExisting()).toBe(true);
  });

  it("should show the deadzone panel", async () => {
    const dzPanel = await $("#panel-deadzone");
    expect(await dzPanel.isExisting()).toBe(true);
  });
});
