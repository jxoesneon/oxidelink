(async () => {
  const tauri = window.__TAURI__;
  if (!tauri) {
    console.error("Tauri global API not available");
    return;
  }

  const { invoke } = tauri.core;
  const { listen } = tauri.event;

  const $ = (id) => document.getElementById(id);
  const overlay = $("overlay");

  const setHidden = (el, hidden) => el.classList.toggle("hidden", hidden);

  // Load overlay configuration so the frontend can honor opacity/scale/toggles.
  let config = {
    opacity: 0.9,
    scale: 1.0,
    show_battery: true,
    show_profile: true,
    show_fps: false,
  };
  try {
    config = await invoke("get_overlay_config");
  } catch (e) {
    console.error("Failed to load overlay config:", e);
  }

  overlay.style.setProperty("--opacity", config.opacity ?? 0.9);
  overlay.style.setProperty("--scale", config.scale ?? 1.0);

  // Simple FPS counter driven by requestAnimationFrame.
  let lastTime = performance.now();
  let frameCount = 0;
  const fpsDisplay = $("fps");

  const updateFps = () => {
    if (!config.show_fps) {
      setHidden(fpsDisplay, true);
      requestAnimationFrame(updateFps);
      return;
    }
    frameCount += 1;
    const now = performance.now();
    if (now - lastTime >= 1000) {
      fpsDisplay.textContent = `${frameCount} FPS`;
      frameCount = 0;
      lastTime = now;
    }
    setHidden(fpsDisplay, false);
    requestAnimationFrame(updateFps);
  };
  updateFps();

  // Listen for controller state updates from the backend.
  await listen("overlay-state", (event) => {
    const payload = event.payload || {};
    const state = payload.state || {};
    const profileName = payload.profile_name;

    setHidden($("battery-row"), !config.show_battery);
    setHidden($("profile-row"), !config.show_profile);

    if (config.show_battery) {
      const pct = state.battery_percent ?? 0;
      $("battery-fill").style.width = `${pct}%`;
      $("battery-value").textContent = `${pct}%`;
    }

    if (config.show_profile) {
      const name = profileName || state.active_profile_name || "Default";
      $("profile-value").textContent = name;
    }

    $("connection-value").textContent = state.connected ? "Connected" : "Disconnected";
  });

  // Quick actions. Note: these require the overlay to receive cursor events.
  $("btn-recenter").addEventListener("click", () => {
    invoke("gyro_recenter").catch(console.error);
  });

  $("btn-hide").addEventListener("click", () => {
    invoke("toggle_overlay").catch(console.error);
  });
})();
