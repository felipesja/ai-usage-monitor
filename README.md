# AI Usage Monitor

TUI local, sem dependências externas, para acompanhar limites de múltiplas contas Claude, Codex e Cursor Business.

## Uso

```bash
ai-usage doctor
ai-usage once
ai-usage watch
```

No TUI:

- `r`: atualiza imediatamente;
- `q`: sai.

A atualização acontece em segundo plano; um spinner "atualizando" aparece no canto superior direito enquanto os dados são buscados, sem travar a tela.

O painel é responsivo: cards em duas colunas quando há espaço, uma coluna em janelas estreitas e uma lista condensada com colunas alinhadas (barra, %, tempo restante) em janelas pequenas — ideal para deixar aberto no canto do monitor. Cada frame usa synchronized output (DEC 2026) para compor sem tearing, e um resize repinta a tela inteira para limpar artefatos.

Cada provedor usa a cor da marca (Claude coral, OpenAI verde, Cursor branco) e é identificado pelo e-mail real da conta (buscado automaticamente: Claude via `oauth/profile`, Codex via `id_token`, Cursor via `auth/me`). Quando há mais de uma conta do mesmo provedor, a que **não** está logada na CLI recebe o marcador destacado `◉ STANDBY` em ciano (as sem marcador estão em uso). Os limites aparecem como `Session` (janela de 5h) e `Weekly` (janela de 7 dias).

## Widget de desktop (Windows)

App nativo em Tauri (`widget/`) com a mesma UI do modo compacto do TUI: janela sem bordas, sempre no topo, fora da taskbar, fixa no canto inferior direito, iniciando junto com o Windows. Os dados vêm do mesmo coletor (`ai-usage once --json` via WSL). Ver `widget/README.md` para build e detalhes.

### Notificações de limite

`watch` dispara um toast nativo do Windows quando um limite cruza o alerta (padrão 80%):

```bash
ai-usage watch --alert 90   # alerta a partir de 90%
ai-usage watch --alert 0    # desliga as notificações
```

Cada limite alerta uma única vez ao cruzar o limiar e volta a armar quando o uso cai abaixo dele (ex.: quando a janela de uso renova). Fora do WSL/Windows a notificação é um no-op silencioso. O widget Tauri tem o mesmo comportamento com notificações próprias.

## Cadastrar as duas contas Claude

Sem alterar a autenticação padrão nem afetar sessões abertas:

```bash
ai-usage claude-login claude-2 --email segunda-conta@exemplo.com
```

O comando usa um perfil temporário vazio, importa a sessão e remove o temporário ao terminar.

Alternativamente, para capturar a sessão padrão atualmente ativa:

Entre na primeira conta normalmente e capture a sessão:

```bash
claude auth login
ai-usage claude-add claude-1
```

Repita com a segunda conta:

```bash
claude auth login
ai-usage claude-add claude-2
```

Os perfis ficam separados em `~/.config/ai-usage-monitor/claude/`, com permissões privadas. O monitor renova os tokens de cada perfil de forma independente.

## Cursor Business

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
