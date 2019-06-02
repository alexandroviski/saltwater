use std::collections::{HashSet, VecDeque};
use std::convert::TryFrom;
use std::iter::{Iterator, Peekable};
use std::mem;

use crate::data::{
    ArrayType, Expr, FunctionType, Keyword, Locatable, Location, Qualifiers, Stmt, StorageClass,
    Symbol, Token, Type,
};
use crate::utils::{error, warn};

type Lexeme = Locatable<Result<Token, String>>;

#[derive(Debug)]
pub struct Parser<I: Iterator<Item = Lexeme>> {
    tokens: Peekable<I>,
    pending: VecDeque<Locatable<Result<Stmt, String>>>,
    last_location: Option<Location>,
    current: Option<Locatable<Token>>,
}

impl<I> Parser<I>
where
    I: Iterator<Item = Lexeme>,
{
    pub fn new(iter: I) -> Self {
        Parser {
            tokens: iter.peekable(),
            pending: Default::default(),
            last_location: None,
            current: None,
        }
    }
}

impl<I: Iterator<Item = Lexeme>> Iterator for Parser<I> {
    type Item = Locatable<Result<Stmt, String>>;
    fn next(&mut self) -> Option<Self::Item> {
        self.pending.pop_front().or_else(|| {
            let Locatable {
                data: lexed,
                location,
            } = self.next_token()?;
            match lexed {
                // NOTE: we do not allow implicit int
                // https://stackoverflow.com/questions/11064292
                Token::Keyword(t) if t.is_decl_specifier() => self.declaration(t),
                _ => Some(Locatable {
                    data: Err("not handled".to_string()),
                    location,
                }),
            }
        })
    }
}

impl<I: Iterator<Item = Lexeme>> Parser<I> {
    /* utility functions */
    fn next_token(&mut self) -> Option<Locatable<Token>> {
        if self.current.is_some() {
            mem::replace(&mut self.current, None)
        } else {
            match self.tokens.next() {
                Some(Locatable {
                    data: Ok(token),
                    location,
                }) => {
                    self.last_location = Some(location.clone());
                    Some(Locatable {
                        data: token,
                        location,
                    })
                }
                None => None,
                Some(Locatable {
                    data: Err(err),
                    location,
                }) => {
                    error(&err, &location);
                    self.last_location = Some(location);
                    self.next_token()
                }
            }
        }
    }
    fn peek_token(&mut self) -> Option<&Locatable<Token>> {
        if self.current.is_none() {
            self.current = self.next_token();
        }
        self.current.as_ref()
    }
    fn next_location(&mut self) -> &Location {
        if self.peek_token().is_some() {
            &self.peek_token().unwrap().location
        } else {
            self.last_location
                .as_ref()
                .expect("can't call next_location on an empty file")
        }
    }
    fn match_next(&mut self, next: Token) -> Option<Locatable<Token>> {
        self.match_any(&[next])
    }
    fn match_any(&mut self, choices: &[Token]) -> Option<Locatable<Token>> {
        match self.peek_token() {
            Some(Locatable { data, .. }) => {
                for token in choices {
                    if token == data {
                        return self.next_token();
                    }
                }
                None
            }
            _ => None,
        }
    }
    fn expect(&mut self, next: Token) -> (bool, &Location) {
        match self.peek_token() {
            Some(Locatable { data, .. }) if *data == next => {
                self.next_token();
                (
                    true,
                    self.last_location
                        .as_ref()
                        .expect("last_location should be set whenever next_token is called"),
                )
            }
            Some(Locatable { location, data }) => {
                // since we're only peeking, we can't move the next token
                let (location, message) = (location.clone(), data.to_string());
                self.pending.push_back(Locatable {
                    location,
                    data: Err(format!("expected '{}', got '{}'", next, message)),
                });
                (false, self.next_location())
            }
            None => {
                let location = self
                    .last_location
                    .as_ref()
                    .expect("parser.expect cannot be called at start of program");
                self.pending.push_back(Locatable {
                    location: location.clone(),
                    data: Err(format!("expected '{}', got <end-of-file>", next)),
                });
                (false, location)
            }
        }
    }

    /* grammar functions
     * this parser is a top-down, recursive descent parser
     * and uses a modified version of the ANSI C Yacc grammar published at
     * https://www.lysator.liu.se/c/ANSI-C-grammar-y.html.
     * Differences from the original grammar, if present, are noted
     * before each function.
     */

    /* this is an utter hack
     * NOTE: the reason the return type is so weird (Result<_, Locatable<_>)
     * is because declaration specifiers can never be a statement on their own:
     * the associated location always belongs to the identifier
     *
     * reference grammar:
     * declaration_specifiers
     *  : storage_class_specifier
     *  | storage_class_specifier declaration_specifiers
     *  | type_specifier
     *  | type_specifier declaration_specifiers
     *  | type_qualifier
     *  | type_qualifier declaration_specifiers
     *  ;
     */
    fn declaration_specifiers(
        &mut self,
        start: Keyword,
    ) -> Result<(StorageClass, Qualifiers, Type), Locatable<String>> {
        // TODO: initialization is a mess
        let mut keywords = HashSet::new();
        keywords.insert(start);
        let mut storage_class = StorageClass::try_from(start).ok();
        let mut qualifiers = Qualifiers {
            c_const: start == Keyword::Const,
            volatile: start == Keyword::Volatile,
        };
        let mut ctype = Type::try_from(start).ok();
        let mut signed = if start == Keyword::Signed {
            Some(true)
        } else if start == Keyword::Unsigned {
            Some(false)
        } else {
            None
        };
        let mut errors = vec![];
        // unsigned const int
        while let Some(locatable) = self.peek_token() {
            let keyword = match locatable.data {
                Token::Keyword(k) if k.is_decl_specifier() => k,
                _ => break,
            };
            let locatable = self.next_token().unwrap();
            if keywords.insert(keyword) {
                handle_single_decl_specifier(
                    keyword,
                    &mut storage_class,
                    &mut qualifiers,
                    &mut ctype,
                    &mut signed,
                    &mut errors,
                    locatable.location,
                );
            } else {
                // duplicate
                // we can guess that they just meant to write it once
                if keyword.is_qualifier()
                    || keyword.is_storage_class()
                    || keyword == Keyword::Signed
                    || keyword == Keyword::Unsigned
                {
                    warn(
                        &format!("duplicate declaration specifier '{}'", keyword),
                        &locatable.location,
                    );
                // what is `short short` supposed to be?
                } else if keyword != Keyword::Long {
                    errors.push(Locatable {
                        data: format!("duplicate basic type '{}' in declarator", keyword),
                        location: locatable.location,
                    });
                }
            }
        }
        while errors.len() > 1 {
            let current = errors.pop().unwrap();
            self.pending.push_front(Locatable {
                location: current.location,
                data: Err(current.data),
            });
        }
        if !errors.is_empty() {
            return Err(errors.pop().unwrap());
        }
        let ctype = match ctype {
            Some(Type::Char(ref mut s))
            | Some(Type::Short(ref mut s))
            | Some(Type::Int(ref mut s))
            | Some(Type::Long(ref mut s)) => {
                *s = signed.unwrap_or(true);
                ctype.unwrap()
            }
            Some(_) => ctype.unwrap(),
            None => {
                if signed.is_none() {
                    warn(
                        "type specifier missing, defaults to int",
                        self.next_location(),
                    );
                }
                Type::Int(signed.unwrap_or(true))
            }
        };
        Ok((
            storage_class.unwrap_or(StorageClass::Auto),
            qualifiers,
            ctype,
        ))
    }
    /*
     * function parameters
     *
     * reference grammar:
     *  parameter_type_list:
     *    parameter_list
     *  | parameter_list ',' ELIPSIS
     *  ;
     */
    fn parameter_type_list(&mut self, return_type: Type) -> Type {
        self.expect(Token::LeftParen);
        let return_type = Box::new(return_type);
        let mut params = vec![];
        let mut errs = VecDeque::new();
        if self.match_next(Token::RightParen).is_some() {
            return Type::Function(FunctionType {
                return_type,
                params,
                varargs: false,
            });
        }
        loop {
            if let Some(locatable) = self.match_next(Token::Ellipsis) {
                if params.is_empty() {
                    errs.push_back(Locatable {
                        location: locatable.location,
                        data: Err("ISO C requires a parameter before '...'".to_string()),
                    });
                }
                return Type::Function(FunctionType {
                    return_type,
                    params,
                    varargs: true,
                });
            }
            let (start, location) = match self.peek_token() {
                Some(Locatable {
                    data: Token::Keyword(k),
                    ..
                }) if k.is_decl_specifier() => {
                    let next = self.next_token().unwrap();
                    let k = match next.data {
                        Token::Keyword(k) => k,
                        _ => panic!("peek should never be different from next"),
                    };
                    (k, next.location)
                }
                _ => {
                    errs.push_back(Locatable {
                        location: self.next_location().clone(),
                        data: Err("function parameters require types".to_string()),
                    });
                    (Keyword::Int, self.next_location().clone())
                }
            };
            let (sc, quals, param_type) = self.declaration_specifiers(start).unwrap_or((
                Default::default(),
                Default::default(),
                Type::Int(true),
            ));
            let possible_type = self.declarator(&param_type, true);
            let (param_name, param_type) = match possible_type.data {
                Err(x) => {
                    errs.push_back(Locatable {
                        location: possible_type.location,
                        data: Err(x),
                    });
                    (None, None)
                }
                Ok((Some(id), param_type)) => (Some(id), Some(param_type)),
                Ok((None, param_type)) => (None, Some(param_type)),
            };
            // NOTE: we are more liberal here than gcc or clang,
            // we allow `int f(auto int);`
            if sc != StorageClass::Auto {
                errs.push_back(Locatable {
                    location, // TODO: use the location of 'start',
                    data: Err(format!(
                        "cannot specify storage class '{}' for {}",
                        sc,
                        match param_name {
                            Some(ref name) => format!("parameter {}", name),
                            None => "unnamed parameter".to_string(),
                        }
                    )),
                });
            }
            if let Some(ctype) = param_type {
                params.push(Symbol {
                    // I will probably regret this in the future
                    // default() for String is "",
                    // which can never be passed in by the lexer
                    // this also makes checking if the parameter is abstract or not
                    // easy to check
                    id: param_name.unwrap_or_default(),
                    ctype,
                    qualifiers: quals,
                    storage_class: StorageClass::Auto,
                });
            }
            if self.match_next(Token::Comma).is_none() {
                self.expect(Token::RightParen);
                // TODO: handle errors (what should the return type be?)
                //let err = errs.pop_front();
                self.pending.append(&mut errs);
                //err.unwrap_or(
                return Type::Function(FunctionType {
                    return_type,
                    params,
                    varargs: false,
                });
            }
        }
    }
    /*
     * not in original reference, see comments to next function
     */
    fn postfix_type(
        &mut self,
        mut prefix: Locatable<(Option<String>, Type)>,
    ) -> Locatable<Result<(Option<String>, Type), String>> {
        // postfix
        while let Some(Locatable { data, .. }) = self.peek_token() {
            prefix.data.1 = match data {
                // array
                Token::LeftBracket => {
                    self.expect(Token::LeftBracket);
                    if self.match_next(Token::RightBracket).is_some() {
                        Type::Array(Box::new(prefix.data.1), ArrayType::Unbounded)
                    } else {
                        let expr = self.parse_expr();
                        self.expect(Token::RightBracket);
                        Type::Array(Box::new(prefix.data.1), ArrayType::Fixed(Box::new(expr)))
                    }
                }
                Token::LeftParen => self.parameter_type_list(prefix.data.1),

                _ => break,
            };
        }
        Locatable {
            location: prefix.location,
            data: Ok(prefix.data),
        }
    }
    /* parse everything after declaration specifiers. can be called recursively
     * allow_abstract: whether to require identifiers in declarators.
     * NOTE: whenever allow_abstract is `false`,
     *  either an identifier or an error will be returned.
     * when allow_abstract is `true`, an identifier may or may not be returned.
     */
    fn declarator(
        &mut self,
        ctype: &Type,
        allow_abstract: bool,
    ) -> Locatable<Result<(Option<String>, Type), String>> {
        if let Some(Locatable { data, location }) = self.peek_token() {
            let prefix = match data {
                Token::LeftParen => {
                    self.next_token();
                    let next = self.declarator(ctype, allow_abstract);
                    self.expect(Token::RightParen);
                    match next.data {
                        Ok(tuple) => Locatable {
                            location: next.location,
                            data: tuple,
                        },
                        Err(_) => return next,
                    }
                }
                Token::Star => {
                    self.next_token();
                    let mut qualifiers = Qualifiers::NONE;
                    while let Some(Locatable {
                        location,
                        data: Token::Keyword(keyword),
                    }) = self.match_any(&[
                        Token::Keyword(Keyword::Const),
                        Token::Keyword(Keyword::Volatile),
                    ]) {
                        if keyword == Keyword::Const {
                            if qualifiers.c_const {
                                warn("duplicate 'const' declaration specifier", &location);
                            } else {
                                qualifiers.c_const = true;
                            }
                        } else if keyword == Keyword::Volatile {
                            if qualifiers.volatile {
                                warn("duplicate 'volatile' declaration specifier", &location);
                            } else {
                                qualifiers.volatile = true;
                            }
                        }
                    }
                    match self.declarator(ctype, allow_abstract) {
                        Locatable {
                            location,
                            data: Ok((id, ctype)),
                        } => Locatable {
                            location,
                            data: (id, Type::Pointer(Box::new(ctype), qualifiers)),
                        },
                        x => return x,
                    }
                }
                Token::Id(_) => {
                    let Locatable { location, data } = self.next_token().unwrap();
                    let id = match data {
                        Token::Id(id) => id,
                        _ => panic!("how could peek return something different from next?"),
                    };
                    Locatable {
                        location,
                        data: (Some(id), ctype.clone()),
                    }
                }
                // TODO: this doesn't look right
                x => {
                    if allow_abstract {
                        Locatable {
                            // this location should never be used
                            location: location.clone(),
                            data: (None, ctype.clone()),
                        }
                    } else {
                        return Locatable {
                            location: location.clone(),
                            data: Err(format!("expected '(', '*', or identifier, got '{}'", x)),
                        };
                    }
                }
            };
            self.postfix_type(prefix)
        } else {
            Locatable {
                location: self.next_location().clone(),
                data: Err("expected type, got <end-of-file>".to_string()),
            }
        }
    }
    // NOTE: there's some fishiness here. Declarations can have multiple variables,
    // but we typed them as only having one Symbol. Wat do?
    // We push all but one declaration into the 'pending' vector
    // and return the last.
    fn declaration(&mut self, start: Keyword) -> Option<Locatable<Result<Stmt, String>>> {
        let (sc, qualifiers, ctype) = match self.declaration_specifiers(start) {
            Ok(tuple) => tuple,
            Err(err) => {
                return Some(Locatable {
                    data: Err(err.data),
                    location: err.location,
                });
            }
        };
        while self.match_next(Token::Semicolon).is_none() {
            let Locatable { location, data } = self.declarator(&ctype, false);
            match data {
                Ok(decl) => {
                    self.pending.push_back(Locatable {
                        location,
                        data: Ok(Stmt::Declaration(Symbol {
                            storage_class: sc,
                            qualifiers: qualifiers.clone(),
                            ctype: decl.1,
                            id: decl.0.expect(
                                "declarator should return id if called with allow_abstract: false",
                            ),
                        })),
                    });
                }
                Err(err) => {
                    self.pending.push_back(Locatable {
                        location,
                        data: Err(err),
                    });
                }
            }
            if self.match_next(Token::Comma).is_none() {
                self.expect(Token::Semicolon);
                break;
            }
        }
        // this is empty when we had specifiers without identifiers
        // e.g. `int;`
        self.pending.pop_front().or_else(|| {
            warn(
                "declaration does not declare anything",
                self.next_location(),
            );
            self.next()
        })
    }
    fn parse_expr(&mut self) -> Expr {
        // TODO: oh honey
        self.next_token();
        Expr::Int(Token::Int(10))
    }
}

#[inline]
/* the reason this is such a mess (instead of just putting everything into
 * the hashmap, which would be much simpler logic) is so we have a Location
 * to go with every error
 * INVARIANT: keyword has not been seen before (i.e. not a duplicate)
 */
fn declaration_specifier(
    keyword: Keyword,
    storage_class: &mut Option<StorageClass>,
    qualifiers: &mut Qualifiers,
    ctype: &mut Option<Type>,
    signed: &mut Option<bool>,
    errors: &mut Vec<Locatable<String>>,
    location: Location,
) {
    // we use `if` instead of `qualifiers.x = keyword == y` because
    // we don't want to reset it if it's already true
    if keyword == Keyword::Const {
        qualifiers.c_const = true;
    } else if keyword == Keyword::Volatile {
        qualifiers.volatile = true;
    } else if keyword == Keyword::Signed || keyword == Keyword::Unsigned {
        if *ctype == Some(Type::Float) || *ctype == Some(Type::Double) {
            errors.push(Locatable {
                data: format!(
                    "invalid modifier '{}' for '{}'",
                    keyword,
                    ctype.as_ref().unwrap()
                ),
                location: location.clone(),
            });
        }
        if *signed == None {
            *signed = Some(keyword == Keyword::Signed);
        } else {
            errors.push(Locatable {
                data: "types cannot be both signed and unsigned".to_string(),
                location,
            });
        }
    } else if let Ok(sc) = StorageClass::try_from(keyword) {
        if *storage_class == None {
            *storage_class = Some(sc);
        } else {
            errors.push(Locatable {
                data: format!(
                    "multiple storage classes in declaration \
                     ('{}' and '{}')",
                    storage_class.unwrap(),
                    sc
                ),
                location,
            });
        }
    } else if keyword == Keyword::Float || keyword == Keyword::Double {
        if *signed != None {
            let s = if signed.unwrap() {
                "signed"
            } else {
                "unsigned"
            };
            errors.push(Locatable {
                data: format!("invalid modifier '{}' for '{}'", s, keyword),
                location,
            });
        } else {
            match ctype {
                None => {}
                Some(Type::Long(_)) if keyword == Keyword::Double => {}
                Some(x) => errors.push(Locatable {
                    data: format!("cannot combine '{}' with '{}'", keyword, x),
                    location,
                }),
            }
            *ctype = Some(Type::try_from(keyword).unwrap());
        }
    } else if keyword == Keyword::Void {
        match ctype {
            Some(x) => errors.push(Locatable {
                data: format!("cannot combine 'void' with '{}'", x),
                location,
            }),
            None => *ctype = Some(Type::Void),
        }
    // if we get this far, keyword is an int type (char - long)
    } else if keyword == Keyword::Int {
        match ctype {
            Some(Type::Char(_)) | Some(Type::Short(_)) | Some(Type::Long(_))
            | Some(Type::Int(_)) => {}
            Some(x) => errors.push(Locatable {
                data: format!("cannot combine 'int' with '{}'", x),
                location,
            }),
            None => *ctype = Some(Type::Int(true)),
        }
    } else {
        match ctype {
            None | Some(Type::Int(_)) => {
                *ctype = Some(
                    Type::try_from(keyword)
                        .expect("keyword should be an integer or integer modifier"),
                )
            }
            Some(x) => errors.push(Locatable {
                data: format!("cannot combine '{}' modifier with type '{}'", keyword, x),
                location,
            }),
        }
    }
}

impl Keyword {
    fn is_qualifier(self) -> bool {
        self == Keyword::Const || self == Keyword::Volatile
    }
    fn is_storage_class(self) -> bool {
        StorageClass::try_from(self).is_ok()
    }
    fn is_decl_specifier(self) -> bool {
        use Keyword::*;
        match self {
            Unsigned | Signed | Void | Bool | Char | Short | Int | Long | Float | Double
            | Extern | Static | Auto | Register | Const | Volatile => true,
            _ => false,
        }
    }
}

impl TryFrom<Keyword> for Type {
    type Error = ();
    fn try_from(keyword: Keyword) -> Result<Type, ()> {
        use Type::*;
        match keyword {
            Keyword::Void => Ok(Void),
            Keyword::Bool => Ok(Bool),
            Keyword::Char => Ok(Char(true)),
            Keyword::Short => Ok(Short(true)),
            Keyword::Int => Ok(Int(true)),
            Keyword::Long => Ok(Long(true)),
            Keyword::Float => Ok(Float),
            Keyword::Double => Ok(Double),
            _ => Err(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Parser;
    use crate::data::{Expr, FunctionType, Locatable, Stmt, Token, Type};
    use crate::Lexer;
    type ParseType = Locatable<Result<Stmt, String>>;
    fn parse(input: &str) -> Option<ParseType> {
        parse_all(input).get(0).cloned()
    }
    fn parse_all(input: &str) -> Vec<ParseType> {
        Parser::new(Lexer::new("<test suite>".to_string(), input.chars())).collect()
    }
    fn match_data<T>(lexed: Option<ParseType>, closure: T) -> bool
    where
        T: Fn(Result<Stmt, String>) -> bool,
    {
        match lexed {
            Some(result) => closure(result.data),
            None => false,
        }
    }
    fn match_type(lexed: Option<ParseType>, given_type: Type) -> bool {
        match_data(lexed, |data| match data {
            Ok(Stmt::Declaration(symbol)) => symbol.ctype == given_type,
            _ => false,
        })
    }
    #[test]
    fn test_decl_specifiers() {
        assert!(match_type(parse("char i;"), Type::Char(true)));
        assert!(match_type(parse("unsigned char i;"), Type::Char(false)));
        assert!(match_type(parse("signed short i;"), Type::Short(true)));
        assert!(match_type(parse("unsigned short i;"), Type::Short(false)));
        assert!(match_type(parse("long i;"), Type::Long(true)));
        assert!(match_type(parse("long long i;"), Type::Long(true)));
        assert!(match_type(parse("long unsigned i;"), Type::Long(false)));
        assert!(match_type(parse("int i;"), Type::Int(true)));
        assert!(match_type(parse("signed i;"), Type::Int(true)));
        assert!(match_type(parse("unsigned i;"), Type::Int(false)));
        assert!(match_type(parse("float f;"), Type::Float));
        assert!(match_type(parse("double d;"), Type::Double));
        assert!(match_type(parse("long double d;"), Type::Double));
        assert!(match_type(
            parse("void f();"),
            Type::Function(FunctionType {
                return_type: Box::new(Type::Void),
                params: vec![],
                varargs: false
            })
        ));
        assert!(match_type(parse("const volatile int f;"), Type::Int(true)));
    }
    #[test]
    fn test_bad_decl_specs() {
        assert!(parse("int;").is_none());
        assert!(parse("char char;").unwrap().data.is_err());
        assert!(parse("char long;").unwrap().data.is_err());
        assert!(parse("long char;").unwrap().data.is_err());
        assert!(parse("float char;").unwrap().data.is_err());
        assert!(parse("float double;").unwrap().data.is_err());
        assert!(parse("double double;").unwrap().data.is_err());
        assert!(parse("double unsigned;").unwrap().data.is_err());
        assert!(parse("short double;").unwrap().data.is_err());
        assert!(parse("int void;").unwrap().data.is_err());
        assert!(parse("void int;").unwrap().data.is_err());
        // default to int if we don't have a type
        // don't panic if we see duplicate specifiers
        assert!(match_type(parse("unsigned unsigned i;"), Type::Int(false)));
        assert!(match_type(parse("extern extern i;"), Type::Int(true)));
        assert!(match_type(parse("const const i;"), Type::Int(true)));
        assert!(match_type(parse("const volatile i;"), Type::Int(true)));
    }
    #[test]
    fn test_complex_types() {
        // this is all super ugly
        use crate::data::{ArrayType, Qualifiers};
        use std::boxed::Box;
        use Type::*;
        assert!(match_type(
            parse("int a[]"),
            Array(Box::new(Int(true)), ArrayType::Unbounded)
        ));
        assert!(match_type(
            parse("unsigned a[]"),
            Array(Box::new(Int(false)), ArrayType::Unbounded)
        ));
        assert!(match_type(
            parse("_Bool a[][][]"),
            Array(
                Box::new(Array(
                    Box::new(Array(Box::new(Bool), ArrayType::Unbounded)),
                    ArrayType::Unbounded
                )),
                ArrayType::Unbounded
            )
        ));
        assert!(match_type(
            parse("void *a"),
            Pointer(Box::new(Void), Default::default())
        ));
        assert!(match_type(
            parse("float *const a"),
            Pointer(Box::new(Float), Qualifiers::CONST)
        ));
        assert!(match_type(
            parse("double *volatile *const a"),
            Pointer(
                Box::new(Pointer(Box::new(Double), Qualifiers::CONST)),
                Qualifiers::VOLATILE
            )
        ));
        assert!(match_type(
            parse("_Bool *volatile const a"),
            Pointer(Box::new(Bool), Qualifiers::CONST_VOLATILE)
        ));
        // cdecl: declare foo as array 10 of pointer to pointer to int
        assert!(match_type(
            parse("char **foo[10];"),
            Pointer(
                Box::new(Pointer(
                    Box::new(Array(
                        Box::new(Char(true)),
                        ArrayType::Fixed(Box::new(Expr::Int(Token::Int(10))))
                    )),
                    Default::default()
                )),
                Default::default()
            )
        ));
        // cdecl: declare foo as pointer to pointer to array 10 of int
        assert!(match_type(
            parse("int (**foo)[10];"),
            Array(
                Box::new(Pointer(
                    Box::new(Pointer(Box::new(Int(true)), Default::default())),
                    Default::default()
                )),
                ArrayType::Fixed(Box::new(Expr::Int(Token::Int(10))))
            )
        ));
    }

}