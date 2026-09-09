# Push this repo (you only)

Do **not** use Cursor or any agent to push. From this directory:

```bash
gh repo create horus-chat/horus-dev-relay --public --source=. --remote=origin --push
```

Or:

```bash
git remote add origin git@github.com:horus-chat/horus-dev-relay.git
git branch -M main
git push -u origin main
```

Suggested tag after push: `git tag v0.7.0 && git push origin v0.7.0`
