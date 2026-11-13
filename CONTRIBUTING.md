# Contributing Guide

**English** · [한국어](CONTRIBUTING.ko.md)

[Project overview](README.md) · [Security policy](SECURITY.md)

Contributions to DevLink are welcome, including bug fixes, documentation and translations, test improvements, and feature proposals. This guide applies to all DevLink repositories.

## Getting started

- Check existing Issues and open PRs in the repository you want to contribute to for duplicate work.
- Include the version and operating system, reproduction steps, expected behavior, and actual behavior in bug reports.
- For new features or major changes, discuss the problem and scope in an Issue before implementing them.
- External contributors should fork the repository, make changes on a working branch, and submit a PR to the upstream repository.

## Branch conventions

`master` is the production branch, and `develop` is the development integration branch. Do not push directly to either branch; submit changes through PRs.

| Working branch | Branch from | PR target |
| --- | --- | --- |
| `feature/{scope}/{description}` | `develop` | `develop` |
| `release/{version}` | `develop` | `master`, `develop` |
| `hotfix/{scope}/{description}` | `master` | `master`, `develop` |

Choose a `scope` that matches the work area: `front`, `back`, `ai`, `data`, `bigdata`, `spark`, `kafka`, `infra`, or `common`.

| Scope | Area |
| --- | --- |
| `front` | Frontend |
| `back` | Backend |
| `ai` | AI / ML |
| `data` | Data collection / preprocessing / ETL |
| `bigdata` | Large-scale data processing |
| `spark` | Spark / distributed processing |
| `kafka` | Kafka / distributed processing |
| `infra` | Docker / servers / deployment infrastructure |
| `common` | Shared work |

```text
feature/front/login
feature/back/auth-api
release/1.0.0
hotfix/back/auth-error
```

## Commit and PR titles

Use the `type(scope): message` format. Use the same work area for `scope` as in the branch name.

Choose a `type` that matches the purpose of the change: `feat`, `fix`, `refactor`, `perf`, `docs`, `test`, `style`, `chore`, or `data`.

| Type | Meaning |
| --- | --- |
| `feat` | Add a feature |
| `fix` | Fix a bug |
| `refactor` | Improve code structure |
| `perf` | Improve performance |
| `docs` | Update documentation |
| `test` | Add or update tests |
| `style` | Change code without affecting behavior |
| `chore` | Configuration, packages, builds, and other maintenance |
| `data` | Dataset or preprocessing changes |

```text
feat(front): add login screen
fix(back): fix authentication error
docs(common): improve installation instructions
```

## Submitting PRs and reviews

Keep each PR focused on one purpose and include:

- The problem addressed, the resulting behavior, and related Issues
- The check and test commands you ran and their results, or why you could not run them
- Updates to relevant examples and documentation for API, configuration, or interface changes

For documentation-only changes, checking the content and links is sufficient. When fixing a bug, add a regression test where possible. If both Korean and English documentation exist, keep the relevant content consistent in both languages.

Do not include passwords, tokens, personal configuration, or unnecessary generated files. Do not post detailed reproduction information for security vulnerabilities in public Issues or PRs.

When reviewers request changes, address them on the same branch and reply with an explanation of the updates. Maintainers will review the changes and verification results before deciding whether to merge.
