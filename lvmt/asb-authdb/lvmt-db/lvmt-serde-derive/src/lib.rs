extern crate proc_macro;
extern crate proc_macro2;
extern crate syn;
#[macro_use]
extern crate quote;

use proc_macro::TokenStream;
use proc_macro2::Span;
use proc_macro2::TokenStream as TokenStream2;
use proc_macro_crate::{crate_name, FoundCrate};
use syn::{Data, DeriveInput, Ident, Index, Member, Type, TypePath};

fn parse_members<'a>(ast: &'a DeriveInput) -> impl Iterator<Item = Member> + 'a {
    if let Data::Struct(ref s) = ast.data {
        s.fields.iter().enumerate().map(|(index, field)| {
            field
                .ident
                .as_ref()
                .map_or(Member::Unnamed(Index::from(index)), |x: &Ident| {
                    Member::Named(x.clone())
                })
        })
    } else {
        panic!("#[derive(MyFromBytes)] is only defined for structs.")
    }
}

fn is_vec<'a>(ast: &'a DeriveInput) -> impl Iterator<Item = bool> + 'a {
    if let Data::Struct(ref s) = ast.data {
        s.fields.iter().map(|field| {
            if let Type::Path(ref path) = field.ty {
                if path.path.segments.len() != 1 {
                    false
                } else {
                    let ty = path.path.segments.first().unwrap();
                    let is_vec = ty.ident.to_string() == "Vec";
                    let vec_u8: TypePath = syn::parse(quote! { Vec<u8> }.into()).unwrap();
                    let is_u8 = path == &vec_u8;
                    is_vec && !is_u8
                }
            } else {
                false
            }
        })
    } else {
        panic!("#[derive(MyFromBytes)] is only defined for structs.")
    }
}

fn dummy_impl(input: TokenStream2, trait_name: &'static str, struct_name: &Ident) -> TokenStream2 {
    let dummy_const = syn::Ident::new(
        &format!("_IMPL_AMT_{}_FOR_{}", trait_name, struct_name),
        struct_name.span(),
    );
    let amt_crate = find_crate();
    quote! {
        #[allow(non_upper_case_globals, unused_attributes, unused_qualifications)]
        const #dummy_const: () = {
            use #amt_crate::serde::{MyFromBytes, MyToBytes, SerdeType};
            #input
        };
    }
}

fn find_crate() -> TokenStream2 {
    let found_crate = crate_name("lvmt-db").expect("lvmt-db is not present in `Cargo.toml`");

    match found_crate {
        FoundCrate::Itself => quote!(crate),
        FoundCrate::Name(name) => {
            let ident = Ident::new(&name, Span::call_site());
            quote!( #ident )
        }
    }
}

#[proc_macro_derive(MyFromBytes)]
pub fn decodable(input: TokenStream) -> TokenStream {
    let ast = syn::parse(input).unwrap();
    let member = parse_members(&ast);
    let read_fn = is_vec(&ast).map(|is_vec| {
        if is_vec {
            quote! { read_vec }
        } else {
            quote! { read }
        }
    });
    let name = &ast.ident;
    let (impl_generics, ty_generics, where_clause) = ast.generics.split_for_impl();

    let impl_block = quote! {
        impl #impl_generics MyFromBytes for #name #ty_generics #where_clause {
            fn read<R: ::std::io::Read>(mut reader: R, ty: SerdeType) -> ::std::io::Result<Self> {
                Ok(Self {
                    #(#member: MyFromBytes::#read_fn(&mut reader, ty)?,)*
                })
            }
        }
    };

    dummy_impl(impl_block, "FROM_BYTES", name).into()
}

#[proc_macro_derive(MyToBytes)]
pub fn encodable(input: TokenStream) -> TokenStream {
    let ast = syn::parse(input).unwrap();
    let member = parse_members(&ast);
    let write_fn = is_vec(&ast).map(|is_vec| {
        if is_vec {
            quote! { write_vec }
        } else {
            quote! { write }
        }
    });
    let name = &ast.ident;
    let (impl_generics, ty_generics, where_clause) = ast.generics.split_for_impl();

    let impl_block = quote! {
        impl #impl_generics MyToBytes for #name #ty_generics #where_clause {
            fn write<W: ::std::io::Write>(&self, mut writer: W, ty: SerdeType) -> ::std::io::Result<()> {
                #(MyToBytes::#write_fn(&self.#member, &mut writer, ty)?;)*
                Ok(())
            }
        }
    };

    dummy_impl(impl_block, "TO_BYTES", name).into()
}
