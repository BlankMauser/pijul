# piclaude — agents Claude en parallèle sur Pijul, avec validation des patchs

Faire tourner plusieurs sessions Claude Code en parallèle sur un même dépôt
Pijul sans que leurs modifications se mélangent, et **valider les patchs avant de
les enregistrer**.

Un seul outil : `contrib/agents/piclaude`. Mets-le sur ton PATH.

## Pré-requis

`piclaude` utilise le `pijul` du PATH, qui doit supporter `record --from-change`
(le patch de carve, dans `pijul/src/commands/record.rs`). Vérifie avec :

```sh
piclaude doctor          # pijul utilisé + capacités éditeur
```

Sinon, `cargo install --path pijul` une fois le patch enregistré, ou pointe
`PICLAUDE_PIJUL` sur le binaire patché.

## Pourquoi forker

`pijul record` diffe la working copy vs le pristine. Deux agents dans un même
arbre mélangent leurs éditions en un hunk indécomposable (provenance perdue). Un
**fork par agent** (working copy + pristine indépendants) transforme les
recouvrements en **conflits Pijul explicites** à l'intégration — qu'on résout, au
lieu de les mélanger. Le fork copie aussi `target/` en **reflink CoW** (btrfs/xfs)
pour un cache de compilation chaud ; fallback en copie complète ailleurs.

## Cycle

```sh
# 1. Lancer un agent : fork isolé + Claude dedans (pijul patché sur PATH)
piclaude                      # nom auto (w1, w2, …)
piclaude nom-de-feature       # ou un nom explicite
piclaude --task "fix the zombie bug"   # slugifie en nom de fork ET sert de
                              #   prompt initial à Claude — une seule ligne
piclaude fork nom             # fork seul, imprime le chemin (pas de lancement)

# 2. Enregistrer, dans la session Claude du fork — deux saveurs :
#   • Saveur 2 (défaut) : demande à Claude d'enregistrer. Le skill piclaude-carve
#     découpe le travail en changes logiques, tu valides en AskUserQuestion, il
#     record chaque groupe via `pijul record --from-change`.
#   • Saveur 1 (échappatoire) : l'éditeur Pijul réel, pour élaguer à la main.
piclaude carve                #   depuis ton terminal (ou `! piclaude carve`)
#   Éditeur graphique (emacs) : pas besoin de TTY, Claude peut l'ouvrir lui-même.
piclaude save -m "message" fichier...   # record -a des SEULS fichiers nommés

# 3. Intégrer les forks dans le dépôt principal (quand les changes sont faits)
piclaude integrate            # pull tous les forks + rapport de conflits
```

L'intégration est **commutative** : l'ordre des forks est indifférent. Les
conflits atterrissent comme marqueurs `>>>>>>>` / `<<<<<<<`, à résoudre une fois
puis `pijul record -a -m 'resolve'`. Le retry sur `PristineLocked` est absorbé.

## Superviser plusieurs agents

Le workflow reste **N terminaux tuilés** sous sway/i3 : c'est la meilleure vue
d'ensemble pour surveiller plusieurs agents. Trois aides pour ne plus avoir à
les scruter en boucle :

```sh
piclaude status               # tableau : par fork, l'état de l'agent
                              # (waiting/ready/running), un diff en attente ?,
                              # et le nb de changes en avance sur main
```

- **Pings automatiques.** `piclaude` injecte des hooks (`piclaude-notify.sh`) qui,
  quand un agent **attend une réponse** (`waiting`) ou **a fini son tour**
  (`ready`), envoient un `notify-send` et marquent la fenêtre **urgent** dans
  sway/i3 (le titre est `piclaude <nom>`). Tu laisses les terminaux tourner ;
  c'est l'agent bloqué qui te tire à lui. Les mêmes hooks écrivent l'état lu par
  `piclaude status` (dans `<fork>/.pijul/piclaude-state`, invisible du working
  tree). Désactiver : `PICLAUDE_NO_STATUS=1`.

- **Lancer sans friction depuis sway/i3.** `piclaude-sway-new.sh` demande la
  tâche (wofi/fuzzel/dmenu), ouvre un terminal titré et lance
  `piclaude new --task "…"` dedans. Un raccourci dans ta config sway :

  ```
  bindsym $mod+Shift+a exec PICLAUDE_REPO=/chemin/vers/le/depot \
      /chemin/vers/contrib/agents/piclaude-sway-new.sh
  ```

  Terminal choisi via `$PICLAUDE_TERMINAL`/`$TERMINAL`, sinon
  foot/alacritty/kitty/… (le premier trouvé).

## Le brief injecté (`piclaude-brief.md`)

Au lancement, `piclaude` ajoute un court brief au system prompt de l'agent
(`--append-system-prompt`) : où il se trouve, comment le travail sort du fork, et
la discipline d'enregistrement Pijul. Le texte vit dans **`piclaude-brief.md`**
(à côté du script) — fichier unique, partagé tel quel avec l'extension VS Code
**pijul/claude**, pour que les deux injectent exactement le même wording.

Deux placeholders y sont substitués au moment du lancement :

| Placeholder      | Remplacé par                          |
|------------------|---------------------------------------|
| `{{WORKSPACE}}`  | le chemin du fork (working copy isolée) |
| `{{MAIN_REPO}}`  | le dépôt principal (où l'on compile)  |

Contrat pour tout consommateur (script shell **ou** extension) : lire le fichier,
remplacer littéralement ces deux jetons, injecter le résultat. Rien d'autre n'est
interprété. Désactiver l'injection : `PICLAUDE_NO_BRIEF=1`.

## Le carve, en une phrase

Le fork est la frontière entre **merge** (entre forks, on préserve les conflits)
et **carve** (dans un fork, un seul auteur, on décompose en features). Le carve
n'est légitime qu'intra-fork. Détails du skill : `.claude/skills/piclaude-carve/`.
