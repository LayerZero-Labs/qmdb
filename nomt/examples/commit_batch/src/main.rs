fn main() -> anyhow::Result<()> {
    commit_batch::NomtDB::commit_batch().map(|_| ())
}
