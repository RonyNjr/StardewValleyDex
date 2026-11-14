# Instruções para agentes

## Linguagem do produto: Rust

- Toda lógica que o app usa em execução fica em Rust.
- Python só para scripts de desenvolvimento descartáveis. Se algo vai rodar mais de uma vez
  ou precisa existir no app, implemente em Rust.
- Ferramentas de linha de comando do projeto ficam em `src-tauri/examples/`
  (ex.: `cargo run --release --example dump`), não em `src-tauri/src/bin/`: o `tauri build`
  empacota nos instaladores todo binário que estiver em `src/bin`.
- A lógica da wiki (HTTP, reescrita de HTML, download de assets) fica em `src-tauri/src/wiki/`
  e é usada tanto pelo dump quanto pelo sync. Não crie uma segunda implementação.

## Comandos

- Testes: `cd src-tauri && cargo test` (não acessam a rede; usam um servidor HTTP local).
- App em desenvolvimento: `npm run tauri dev`.
