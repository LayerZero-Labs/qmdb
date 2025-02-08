# GNU Time Parser Action

A GitHub Action that parses the output of GNU's `/usr/bin/time -v` command and converts it into structured JSON data.

## Usage

Add this action to your workflow:

```yaml
- uses: ./.github/actions/parse-gnu-time
  with:
    content: ${{ steps.bench.outputs.time_output }}
```

## Development

### Install Dependencies
```bash
npm install
```

### Run Tests
```bash
npm test
```
