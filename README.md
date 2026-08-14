# 🌾 StardewValleyDex

> Wiki completa de Stardew Valley disponível **offline**, em Português — app desktop construído com Tauri + Rust.

---

## ⬇️ Download

Baixe o instalador para o seu sistema na página de [Releases](https://github.com/RonyNjr/StardewValleyDex/releases/latest):

| Sistema | Arquivo |
|---------|---------|
| Windows | `.msi` ou `.exe` |
| macOS   | `.dmg` |
| Linux   | `.AppImage` |

> Na primeira execução o app baixa automaticamente os dados da wiki (~70 MB). Depois disso funciona 100% offline.

---

## 📌 Sobre o Projeto

**StardewValleyDex** é um aplicativo desktop que armazena localmente o conteúdo da [Wiki de Stardew Valley em Português](https://pt.stardewvalleywiki.com). Navegue por todas as páginas, tabelas, imagens e guias instantaneamente, sem depender de internet.

### Funcionalidades

- 🔍 **Busca instantânea** por qualquer página da wiki
- 📖 **Navegação offline** completa — sem internet após o setup
- 🌐 **Conteúdo em Português** com fallback para o inglês
- ⬅️ **Histórico de navegação** com botão voltar
- 🔗 **Links externos** abrem no navegador padrão
- 🎨 **Header personalizado** com design moderno integrado à wiki

---

## 🛠️ Tecnologias

| Camada | Tecnologia |
|--------|------------|
| Framework desktop | [Tauri 2](https://tauri.app) (Rust) |
| Backend / protocolo | Rust (`wiki://` custom scheme) |
| Banco de dados | SQLite (páginas HTML + metadados) |
| Frontend | TypeScript + Vite |
| Scraper (build tool) | Python + BeautifulSoup |

---

## ⚙️ Como funciona

1. O script `scripts/dump.py` faz o scraping da wiki PT (com fallback EN) e salva tudo em SQLite
2. O app Tauri serve as páginas via protocolo customizado `wiki://localhost/NomeDaPágina`
3. O header customizado é injetado em cada página servida pelo backend Rust
4. Na primeira execução em uma máquina nova, o app baixa o zip de dados do GitHub Releases e importa automaticamente

---

## 🏗️ Estrutura do Projeto

```
StardewValleyDex/
├── src/               # Frontend TypeScript
├── src-tauri/         # Backend Rust (Tauri)
│   ├── src/
│   │   ├── lib.rs     # Comandos Tauri (setup, navegação)
│   │   ├── protocol.rs # Servidor wiki:// + injeção de header
│   │   └── db.rs      # Queries SQLite
│   └── resources/     # Assets bundled no binário
├── scripts/
│   ├── dump.py        # Scraper principal (--crawl-missing disponível)
│   └── make_release.py # Gera o zip de dados para release
└── .github/workflows/ # CI/CD para builds multi-plataforma
```

---

## ⚠️ Isenção de Responsabilidade

- Projeto **não oficial**, mantido por fã para fins educacionais e de utilidade pessoal.
- **Stardew Valley** é marca registrada da **ConcernedApe LLC**. Sem afiliação.
- Conteúdo da wiki distribuído sob licença **[CC BY-NC-SA 3.0](https://creativecommons.org/licenses/by-nc-sa/3.0/)**.
