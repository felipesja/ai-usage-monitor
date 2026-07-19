# AI Usage Widget — app Tauri

Widget de desktop nativo (Tauri v2 + WebView2) com a UI do modo compacto do TUI: mesmas cores por provedor, barras, `%`, tempo de renovação, marcador `◉ STANDBY`.

## Como funciona

- **Dados**: o Rust mantém uma ponte `wsl.exe` com stdio redirecionado e `CREATE_NO_WINDOW`, sem abrir terminal. O front-end solicita cada leitura por essa ponte e renderiza o JSON — o coletor Python continua sendo a única fonte de verdade.
- **Janela**: sem bordas, sempre no topo, fora da taskbar (`alwaysOnTop`, `decorations: false`, `skipTaskbar`), fixa no canto inferior direito da work area (calculada no `setup` do Rust). Ela nasce oculta e só pode ser mostrada depois que o frontend estiver pronto, evitando o flash branco inicial do WebView.
- **Iniciar com o Windows**: `tauri-plugin-autostart` (entrada no registro, habilitada no primeiro run do build release).
- **Notificações**: `tauri-plugin-notification` quando um limite cruza 80% — uma vez ao cruzar, re-armando quando o uso cai abaixo do limiar (histerese).
- **Tray**: o app inicia oculto, só com o ícone na bandeja. Click esquerdo abre a janela com foco (click de novo esconde); click direito → "Sair" encerra o app e a ponte WSL.
- **Interação**: `r` ou o botão `↻` atualizam; `q`/`Esc`, `✕` ou Alt+F4 escondem para a bandeja; arrastar em qualquer área vazia move a janela (ao reabrir, volta ao canto).

## Compilar

Requisitos no Windows (instaláveis via winget): Rust (rustup, toolchain MSVC), Visual Studio Build Tools 2022 com C++, WebView2 Runtime, Node.js.

O build precisa rodar no filesystem do Windows (cargo não funciona bem em `\\wsl.localhost`). Copie este diretório para um workspace no disco do Windows (ex.: `%USERPROFILE%\dev\ai-usage-widget`) e sincronize as mudanças para lá antes de compilar.

```powershell
cd %USERPROFILE%\dev\ai-usage-widget
npm install
npm run dev      # modo dev
npm run build    # release: src-tauri\target\release\ai-usage-widget.exe + instalador NSIS em ...\release\bundle\nsis\
```

Os ícones em `src-tauri/icons/` já estão gerados; para trocar, use `npx tauri icon caminho\para\icone.png`.
