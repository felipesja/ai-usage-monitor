# AI Usage Monitor

Widget de desktop nativo para **Windows** (Tauri) que mostra, fixo no canto da tela, os limites de uso de múltiplas contas **Claude**, **Codex** e **Cursor Business** — sempre no topo, fora da taskbar e iniciando junto com o Windows.

Começou como um painel de terminal (TUI) e evoluiu para o app nativo, que hoje é a forma principal de uso. O coletor Python — sem dependências externas — continua sendo o motor por trás do widget e também roda sozinho como painel de terminal.

## Widget de desktop (Windows)

App nativo em Tauri v2 + WebView2 (`widget/`):

- **Janela**: sem bordas, sempre no topo, fora da taskbar, fixa no canto inferior direito. Nasce oculta (sem flash branco) e é reposicionada a cada exibição.
- **Bandeja**: inicia só com o ícone na tray. Click esquerdo abre/fecha o widget; click direito → "Sair".
- **Sempre atualizado**: refresh automático em segundo plano com spinner; `r` ou o botão `↻` forçam atualização; `q`/`Esc`/`✕`/Alt+F4 escondem para a bandeja.
- **Notificações**: toast nativo do Windows quando um limite cruza 80% — uma vez ao cruzar, re-armando quando o uso cai abaixo do limiar.
- **Iniciar com o Windows**: entrada de autostart habilitada no primeiro run do build release.

Cada provedor usa a cor da marca (Claude coral, OpenAI verde, Cursor branco) e é identificado pelo e-mail real da conta. Com mais de uma conta do mesmo provedor, a que **não** está logada na CLI recebe o marcador `◉ STANDBY` em ciano. Os limites aparecem como `Session` (janela de 5h) e `Weekly` (janela de 7 dias).

Build e detalhes de implementação: [`widget/README.md`](widget/README.md).

## Como funciona

O widget é **self-contained**: a coleta roda em Rust dentro do próprio app, sem WSL, sem Python e sem processos externos.

```
Tauri (Rust, Windows) → coletor nativo → APIs de Claude, Codex e Cursor
```

As credenciais ficam em `%USERPROFILE%\.config\ai-usage-monitor\`, no mesmo formato usado pelo coletor Python — os dois leem o mesmo store. Como os limites são server-side, os números são idênticos independente de onde a leitura acontece.

Para diagnosticar a coleta sem abrir a janela:

```powershell
ai-usage-widget.exe --probe   # imprime o JSON da coleta e sai
```

## Painel de terminal (opcional)

O coletor Python (`cli/usage_monitor.py`) roda direto no terminal, em Linux/WSL ou Windows, e também é quem cadastra as credenciais:

```bash
ai-usage doctor   # verifica a configuração
ai-usage once     # uma leitura (use --json para saída em JSON)
ai-usage watch    # TUI ao vivo, responsivo, com atualização automática
```

No Windows, chame pelo Python (`python cli\usage_monitor.py <comando>`). O `watch` exige o módulo `curses`, ausente no Python do Windows — lá use `once` ou o widget.

No TUI: `r` atualiza, `q` sai. As notificações também valem aqui:

```bash
ai-usage watch --alert 90   # alerta a partir de 90%
ai-usage watch --alert 0    # desliga as notificações
```

Fora do WSL/Windows a notificação é um no-op silencioso.

## Configurar as contas

### Duas contas Claude

Sem alterar a autenticação padrão nem afetar sessões abertas:

```bash
ai-usage claude-login claude-2 --email segunda-conta@exemplo.com
```

O comando usa um perfil temporário vazio, importa a sessão e remove o temporário ao terminar.

Alternativamente, para capturar a sessão padrão atualmente ativa (repita para cada conta):

```bash
claude auth login
ai-usage claude-add claude-1
```

Os perfis ficam separados em `~/.config/ai-usage-monitor/claude/`, com permissões privadas. O monitor renova os tokens de cada perfil de forma independente.

### Cursor Business

Opção preferencial para administradores da equipe:

```bash
ai-usage cursor-admin --email seu-email@empresa.com
```

A chave é lida por prompt oculto. Ela pode ser criada em Cursor Dashboard → Settings → Admin API Keys.

Sem permissão administrativa, use o cookie `WorkosCursorSessionToken` do dashboard:

```bash
ai-usage cursor-cookie
```

Essa alternativa usa o endpoint interno do dashboard e pode precisar de ajuste se o Cursor o alterar.

## Segurança

- Tokens, cookies e chaves nunca aparecem no painel ou em argumentos do processo.
- Diretórios de configuração usam permissão `0700`; arquivos de credencial usam `0600`.
- Não copie credenciais para chats, issues ou repositórios.

## Licença

MIT — ver [LICENSE](LICENSE).
