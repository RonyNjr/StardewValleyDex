import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

// Data package built with `cargo run --release --example dump -- --zip ...`
// and published as a GitHub release asset. Its format must match
// `DATA_VERSION` in src-tauri/src/db.rs.
const DATA_URL =
  "https://github.com/RonyNjr/StardewValleyDex/releases/download/dados-2026-11/stardewvalleydex-data.zip";

type SetupStatus = { has_data: boolean; up_to_date: boolean };

function openWiki() {
  window.location.href = "wiki://localhost/";
}

const STEPS: Record<string, string> = {
  downloading: "A descarregar dados da wiki...",
  extracting: "A extrair arquivo...",
  importing: "A importar páginas...",
  done: "Concluído!",
};

async function main() {
  const setup = await invoke<SetupStatus>("check_setup");
  if (setup.has_data && setup.up_to_date) {
    openWiki();
    return;
  }

  showDownloadUI(setup.has_data);

  const status = document.getElementById("svd-status")!;
  const bar = document.getElementById("svd-bar")!;
  const pct = document.getElementById("svd-pct")!;

  await listen<{ step: string; pct: number }>("setup_progress", (ev) => {
    const { step, pct: p } = ev.payload;
    status.textContent = STEPS[step] ?? step;
    bar.style.width = `${p}%`;
    pct.textContent = `${p}%`;
    if (step === "done") {
      setTimeout(openWiki, 800);
    }
  });

  try {
    await invoke("download_and_setup", { url: DATA_URL });
  } catch (e) {
    bar.style.background = "#e74c3c";
    if (setup.has_data) {
      // Offline or download failed: keep using the pages already stored.
      // The update is tried again on the next start.
      status.textContent = "Não foi possível baixar a atualização. A abrir os dados atuais…";
      setTimeout(openWiki, 2500);
    } else {
      status.textContent = `Erro: ${e}`;
    }
  }
}

function showDownloadUI(updating: boolean) {
  const intro = updating
    ? "A atualizar os dados da wiki (agora com imagens offline)…"
    : "A preparar dados iniciais…";
  document.body.innerHTML = `
    <div style="
      font-family: sans-serif;
      display: flex;
      flex-direction: column;
      align-items: center;
      justify-content: center;
      height: 100vh;
      background: #2d5a1b;
      color: #fff;
      gap: 24px;
      margin: 0;
    ">
      <img src="/src/assets/stardew-icon.png" style="width:72px;height:72px;object-fit:contain;border-radius:12px" onerror="this.style.display='none'">
      <h1 style="margin:0;font-size:2rem;letter-spacing:.5px">StardewValleyDex</h1>
      <p id="svd-status" style="margin:0;font-size:1.05rem;opacity:.9">
        ${intro}
      </p>
      <div style="width:420px;background:rgba(255,255,255,.2);border-radius:8px;overflow:hidden;height:18px">
        <div id="svd-bar" style="height:100%;width:0%;background:#f4c430;transition:width .4s ease"></div>
      </div>
      <p id="svd-pct" style="margin:0;opacity:.7;font-size:.9rem">0%</p>
      <p style="margin:0;opacity:.5;font-size:.8rem;max-width:360px;text-align:center">
        Só acontece uma vez. As páginas ficam guardadas localmente.
      </p>
    </div>
  `;
}

main();
