# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Idioma

Comentários, strings visíveis ao usuário, mensagens de erro e docs são em **português (pt-BR)**. Mantenha esse padrão ao editar.

## Comandos

O coletor é um único script Python sem dependências externas (só stdlib), em `cli/usage_monitor.py`. Instalado como `ai-usage` (symlink `~/.local/bin/ai-usage` → `cli/usage_monitor.py`).

```bash
python3 cli/usage_monitor.py doctor    # verifica configuração (perfis Claude, CLI Codex, Cursor)
python3 cli/usage_monitor.py once      # uma leitura em texto; --json para JSON
python3 cli/usage_monitor.py watch     # abre o TUI curses (--interval N, --alert N)
ai-usage bridge                    # loop stdin→JSON de linha única (consumido pelo widget)
```

Não há suíte de testes, linter ou build para o script Python — é executado direto.

### Widget Tauri (`widget/`)

O build **precisa rodar no filesystem do Windows** (cargo falha em `\\wsl.localhost`). Não compila a partir do WSL. Fluxo (PowerShell, no workspace Windows sincronizado):

```powershell
npm install
npm run dev      # dev
npm run build    # release: .exe + instalador NSIS em src-tauri\target\release\bundle\nsis\
```

Ícones: `npx tauri icon caminho\para\icone.png`.

## Arquitetura

### Coletor Python (`cli/usage_monitor.py`) — fonte única de verdade

Todo o resto (TUI, widget) só renderiza o que este script produz. Fluxo:

- **Modelo de dados:** dataclasses `Provider` (nome, conta, plano, email, `standby`, lista de `Meter`, `error`) e `Meter` (label, percent, reset_at, used, limit). Serializados via `asdict` para `--json`/`bridge`.
- **`collect_all()`** faz fan-out com `ThreadPoolExecutor` sobre: cada perfil Claude em `~/.config/ai-usage-monitor/claude/*/`, mais Codex e Cursor. Cada `collect_*` **captura suas próprias exceções** e retorna um `Provider` com `.error` preenchido — nunca propaga. Depois `mark_standby()` compara os emails Claude com a conta ativa da CLI (`~/.claude.json`) e marca `◉ STANDBY` na que não está logada.
- **Claude:** OAuth com refresh automático de token (`refresh_claude` renova quando falta <2min), lê `oauth/usage` e `oauth/profile`. Multi-conta: perfis isolados em subdiretórios com `.credentials.json`.
- **Codex:** tenta o `app-server` via JSON-RPC sobre stdio (`codex_live`); em falha cai para o cache local das sessões (`codex_cached` lê o último `token_count` em `~/.codex/sessions/*.jsonl`) e sinaliza o downgrade em `details`. `codex_bin()` contorna o PATH: ignora shims de `/mnt/` (Windows) e procura o binário Linux via nvm.
- **Cursor:** dois métodos em `cursor.json` — `admin_key` (API admin de equipe, preferencial) ou `dashboard_cookie` (endpoint interno do dashboard, pode quebrar se o Cursor mudar). O dashboard de equipe divide as unidades de request por 4 (`request_scale`).

### Renderização

- `plain()` — saída texto do `once`.
- `tui()` — TUI curses com **thread de fetch em background** (o loop de desenho nunca bloqueia; spinner enquanto busca). Dois layouts: cards (2 colunas se largura ≥96) e `draw_compact_tui` (lista alinhada) para janelas pequenas. Usa synchronized output DEC 2026 (`\x1b[?2026h/l`) para compor sem tearing e um `redrawwin` no resize para limpar artefatos.
- **Alertas:** `alert_meters` com histerese — notifica 1x ao cruzar o limiar, re-arma só quando o uso cai abaixo. `notify_windows` dispara toast via `powershell.exe` (WinRT); no-op fora do WSL.

### Widget Tauri (`widget/`)

App nativo Tauri v2 + WebView2 (Windows) que espelha o modo compacto do TUI.

- **Coletor nativo (`src-tauri/src/collector/`):** porta em Rust do coletor Python — `claude.rs` (refresh OAuth + usage/profile), `codex.rs` (JSON-RPC no `app-server`, via `codex.cmd` com `CREATE_NO_WINDOW`, com fallback para o cache de sessões), `cursor.rs` (admin_key/dashboard_cookie), `date.rs` (ISO-8601 sem `chrono`), `config.rs` (store em `%USERPROFILE%\.config\ai-usage-monitor\`). Serializa exatamente o mesmo JSON do Python, então o frontend serve aos dois. **Não há mais ponte WSL** — o app é self-contained. `--probe` imprime a coleta e sai, para diagnóstico sem GUI.
- **Duas implementações do mesmo contrato:** ao mudar o formato de `Provider`/`Meter`, atualize o Python (`cli/usage_monitor.py`) e o Rust (`collector/mod.rs`) juntos.
- **Frontend (`src/main.js`, `src/index.html`):** vanilla JS, sem framework, `withGlobalTauri`. Replica a lógica do TUI (cores por provedor, histerese de alertas). Comandos Tauri: `fetch_usage` (async, senão congela a UI), `hide_to_tray`, `frontend_ready`.
- **Janela:** nasce oculta (evita flash branco), sem bordas, always-on-top, fora da taskbar, reposicionada no canto inferior direito a cada show (sem persistência de posição). Tray: click esquerdo alterna, direito → Sair. Autostart e notificações só no build release.

## Segurança

Tokens/cookies/chaves nunca aparecem no painel nem em argumentos de processo. Diretórios de config `0700`, arquivos de credencial `0600` (ver `ensure_private_dir`/`write_private_json`). Nunca copiar credenciais para chats, issues ou commits.
