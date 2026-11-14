# 🌾 StardewValleyDex

> Wiki completa de Stardew Valley disponível **offline**, em Português — app desktop construído com Tauri + Rust.

---

## ⬇️ Download

Baixe o instalador para o seu sistema na página de [Releases](https://github.com/RonyNjr/StardewValleyDex/releases/latest):

| Sistema | Arquivo |
|---------|---------|
| Windows | `.msi` ou `.exe` |
| macOS   | `.dmg` |
| Linux   | `.AppImage`, `.deb` ou `.rpm` |

> Na primeira execução o app baixa automaticamente os dados da wiki (~165 MB, com imagens). Depois disso funciona 100% offline.

### Avisos na primeira instalação

Os instaladores não têm a assinatura paga da Microsoft e da Apple, então o sistema pede uma confirmação na primeira vez:

- **Windows:** se aparecer "O Windows protegeu o computador", clique em **Mais informações** → **Executar assim mesmo**.
- **macOS:** abra o `.dmg` e arraste o app para **Aplicativos**. Se ao abrir aparecer que o app não pôde ser verificado, vá em **Ajustes do Sistema** → **Privacidade e Segurança** e clique em **Abrir Mesmo Assim**. No macOS 14 ou anterior, basta clicar com o botão direito no app → **Abrir**.
- **Linux:** dê permissão de execução ao `.AppImage` (`chmod +x StardewValleyDex_*.AppImage`) ou instale o `.deb`/`.rpm` pelo gerenciador de pacotes.

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
| Dump da wiki (build tool) | Rust (`src-tauri/examples/dump.rs`) |

---

## ⚙️ Como funciona

1. O dump (`src-tauri/examples/dump.rs`) baixa a wiki PT (com fallback EN), reescreve links e imagens para `wiki://localhost/` e salva tudo em SQLite
2. O app Tauri serve as páginas via protocolo customizado `wiki://localhost/NomeDaPágina`
3. O header customizado é injetado em cada página servida pelo backend Rust
4. Na primeira execução em uma máquina nova, o app baixa o zip de dados do GitHub Releases e importa automaticamente. Quando o formato dos dados muda (`DATA_VERSION` em `db.rs`), quem já tem o app baixa o pacote novo uma vez; sem internet, continua com os dados que já tem
5. Com o app aberto, a cada 30 min o sync baixa de novo as páginas editadas na wiki, usando o mesmo código do dump

### Gerar os dados

```bash
cd src-tauri
cargo run --release --example dump -- --limit 5      # teste rápido
cargo run --release --example dump                   # dump completo
cargo run --release --example dump -- --resume --crawl-missing
cargo run --release --example dump -- --help         # todas as opções

# zip de dados do release (o que o app baixa na primeira execução)
cargo run --release --example dump -- --crawl-missing --data-dir ../dump --zip ../stardewvalleydex-data.zip
```

Por padrão os dados vão para a pasta de dados do app (`~/.local/share/com.stardewvalleydex.app` no Linux, `~/Library/Application Support/com.stardewvalleydex.app` no macOS, `%APPDATA%\com.stardewvalleydex.app` no Windows). Use `--data-dir` para outro lugar.

---

## 🏗️ Estrutura do Projeto

```
StardewValleyDex/
├── src/               # Frontend TypeScript
├── src-tauri/         # Backend Rust (Tauri)
│   ├── src/
│   │   ├── lib.rs     # Comandos Tauri (setup, navegação)
│   │   ├── protocol.rs # Servidor wiki:// + injeção de header
│   │   ├── db.rs      # Queries SQLite
│   │   ├── sync.rs    # Atualização periódica das páginas editadas
│   │   └── wiki/      # Download, reescrita de HTML e assets (usado pelo dump e pelo sync)
│   ├── examples/
│   │   └── dump.rs    # Dump inicial da wiki (--crawl-missing disponível)
│   └── resources/     # Assets bundled no binário
└── .github/workflows/ # CI/CD para builds multi-plataforma
```

---

## ⚠️ Isenção de Responsabilidade

- Projeto **não oficial**, mantido por fã para fins educacionais e de utilidade pessoal.
- **Stardew Valley** é marca registrada da **ConcernedApe LLC**. Sem afiliação.
- Conteúdo da wiki distribuído sob licença **[CC BY-NC-SA 3.0](https://creativecommons.org/licenses/by-nc-sa/3.0/)**.
