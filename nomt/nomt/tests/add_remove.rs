mod common;

use common::Test;
use hex_literal::hex;
use nomt::trie::Node;

#[test]
fn add_remove_1000() {
    let mut accounts = 0;
    let mut t = Test::new("add_remove");

    let expected_roots = [
        hex!("0000000000000000000000000000000000000000000000000000000000000000"),
        hex!("4a7a6fe118037086a49ff10484f4d80b0a9f31f1060eeb1c9f0162634604b0d9"),
        hex!("7d5b013105d7b835225256f2233a458e1a158a53d20e0d3834886df89a26c27b"),
        hex!("1a290e07bcacfb58ddcd0b9da348c740ca1bf87b05ed96752a1503ed7c187b69"),
        hex!("5e9abfee6d927b084fed3e1306bbe65f0880d0b7de12522c38813014927f1336"),
        hex!("57b39e06b2ee98dccd882033eb4136f5376699128b421c83bdc7c6ca96168938"),
        hex!("7fd75809ef0e2133102eb5e31e47cb577149dcaebb42cddeb2fd6754256b365f"),
        hex!("7c00cb11ec8262385078613e7b7977e50b0751f8cb2384fdccc048eea02acb63"),
        hex!("516d6911c3b0a36c9227922ca0273a4aee44886201bd186f7ee7e538a769eaa5"),
        hex!("381b24719ff91b13d36cf0dd7622f391f4a461452ed7547a46a992ee4a4025aa"),
        hex!("207793e2ce76c1feb68c7259f883229f985706c8cc2fcf99f481b622a54ba375"),
    ];

    let mut root = Node::default();
    for i in 0..10 {
        let _ = t.read_id(0);
        for _ in 0..100 {
            common::set_balance(&mut t, accounts, 1000);
            accounts += 1;
        }
        {
            root = t.commit().0.into_inner();
        }

        assert_eq!(root, common::expected_root(accounts));
        assert_eq!(root, expected_roots[i + 1]);
    }

    assert_eq!(root, expected_roots[10]);

    for i in 0..10 {
        for _ in 0..100 {
            accounts -= 1;
            common::kill(&mut t, accounts);
        }
        {
            root = t.commit().0.into_inner();
        }

        assert_eq!(root, common::expected_root(accounts));
        assert_eq!(root, expected_roots[10 - i - 1]);
    }
}
