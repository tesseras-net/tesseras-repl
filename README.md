# Tesseras REPL

Interactive REPL for the [Tesseras](https://tesseras.net) DHT network.

## About

Tesseras REPL is an interactive command-line shell for storing and retrieving
content on the Tesseras P2P network. It spawns a local DHT node and provides a
readline-based interface with command completion, history, and an editor
integration for composing multi-line content.

On first run the node generates an Ed25519 identity and performs proof-of-work,
which may take a moment. Subsequent starts are instant. Data is persisted under
`~/.local/share/tesseras/` (or `$XDG_DATA_HOME/tesseras/`).

## Usage

```
tesseras-repl [options]
```

### Options

| Flag                   | Description                |
| ---------------------- | -------------------------- |
| `-4`                   | Only listen on IPv4        |
| `-6`                   | Only listen on IPv6        |
| `-a`, `--address ADDR` | Listen address `ip[@port]` |
| `-v`, `--version`      | Print version and exit     |
| `-h`, `--help`         | Print help and exit        |

### REPL Commands

```
tesseras> help
  put <content>  Store content in the DHT
  get <token>    Retrieve content by token
  edit           Open $EDITOR to compose and store
  list           List known tokens
  info           Show node information
  help           Show this help
  exit           Quit the REPL
```

## Links

- [Website](https://tesseras.net)
- [Documentation](https://tesseras.net/book/en/)
- [Source code](https://git.sr.ht/~ijanc/tesseras-repl) (primary)
- [GitHub mirror](https://github.com/tesseras-net/tesseras-repl)
- [Ticket tracker](https://todo.sr.ht/~ijanc/tesseras)
- [Mailing lists](https://tesseras.net/subscriptions/)

## License

ISC — see [LICENSE](LICENSE).
