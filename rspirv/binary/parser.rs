// Copyright 2016 Google Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use mr;
use grammar;
use spirv;

use std::{error, fmt, result};
use super::decoder;
use super::error::Error as DecodeError;

use grammar::InstructionTable as GInstTable;
use grammar::OperandKind as GOpKind;
use grammar::OperandQuantifier as GOpCount;

type GInstRef = &'static grammar::Instruction<'static>;

/// Parser State.
#[derive(Debug)]
pub enum State {
    /// Parsing completed
    Complete,
    /// Consumer requested to stop parse
    ConsumerStopRequested,
    /// Consumer errored out with the given error
    ConsumerError(Box<error::Error>),
    /// Incomplete module header
    HeaderIncomplete(DecodeError),
    /// Incorrect module header
    HeaderIncorrect,
    /// Unsupported endianness
    EndiannessUnsupported,
    /// Incomplete instruction at (byte offset, inst index)
    InstructionIncomplete(usize, usize),
    /// Zero instruction word count at (byte offset, inst index)
    WordCountZero(usize, usize),
    /// Unknown opcode at (byte offset, inst index, opcode)
    OpcodeUnknown(usize, usize, u16),
    /// Expected more operands (byte offset, inst index)
    OperandExpected(usize, usize),
    /// found redundant operands (byte offset, inst index)
    OperandExceeded(usize, usize),
    /// Errored out when decoding operand with the given error
    OperandError(DecodeError),
}

impl error::Error for State {
    fn description(&self) -> &str {
        match *self {
            State::Complete => "completed parsing",
            State::ConsumerStopRequested => {
                "stop parsing requested by consumer"
            }
            State::ConsumerError(_) => "consumer error",
            State::HeaderIncomplete(_) => "incomplete module header",
            State::HeaderIncorrect => "incorrect module header",
            State::EndiannessUnsupported => "unsupported endianness",
            State::InstructionIncomplete(..) => "incomplete instruction",
            State::WordCountZero(..) => "zero word count found",
            State::OpcodeUnknown(..) => "unknown opcode",
            State::OperandExpected(..) => "expected more operands",
            State::OperandExceeded(..) => "found extra operands",
            State::OperandError(_) => "operand decoding error",
        }
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            State::Complete => write!(f, "completed parsing"),
            State::ConsumerStopRequested => {
                write!(f, "stop parsing requested by consumer")
            }
            State::ConsumerError(ref err) => {
                write!(f, "consumer error: {}", err)
            }
            State::HeaderIncomplete(ref err) => {
                write!(f, "incomplete module header: {}", err)
            }
            State::HeaderIncorrect => write!(f, "incorrect module header"),
            State::EndiannessUnsupported => write!(f, "unsupported endianness"),
            State::InstructionIncomplete(offset, index) => {
                write!(f,
                       "incomplete instruction #{} at offset {}",
                       index,
                       offset)
            }
            State::WordCountZero(offset, index) => {
                write!(f,
                       "zero word count found for instruction #{} at offset {}",
                       index,
                       offset)
            }
            State::OpcodeUnknown(offset, index, opcode) => {
                write!(f,
                       "unknown opcode ({}) for instruction #{} at offset {}",
                       opcode,
                       index,
                       offset)
            }
            State::OperandExpected(offset, index) => {
                write!(f,
                       "expected more operands for instruction #{} at offset \
                        {}",
                       index,
                       offset)
            }
            State::OperandExceeded(offset, index) => {
                write!(f,
                       "found extra operands for instruction #{} at offset {}",
                       index,
                       offset)
            }
            State::OperandError(ref err) => {
                write!(f, "operand decoding error: {}", err)
            }
        }
    }
}

pub type Result<T> = result::Result<T, State>;

const HEADER_NUM_WORDS: usize = 5;
const MAGIC_NUMBER: spirv::Word = 0x07230203;

/// Orders consumer sent to the parser after each consuming call.
#[derive(Debug)]
pub enum Action {
    /// Continue the parsing
    Continue,
    /// Normally stop the parsing
    Stop,
    /// Error out with the given error
    Error(Box<error::Error>),
}

/// The binary consumer trait.
///
/// The parser will call `initialize` before parsing the SPIR-V binary and
/// `finalize` after successfully parsing the whle binary.
///
/// After successfully parsing the module header, `consume_header` will be
/// called. After successfully parsing an instruction, `consume_instruction`
/// will be called.
///
/// The consumer can use [`Action`](enum.ParseAction.html) to control the
/// parsing process.
pub trait Consumer {
    /// Intialize the consumer.
    fn initialize(&mut self) -> Action;
    /// Finalize the consumer.
    fn finalize(&mut self) -> Action;

    /// Consume the module header.
    fn consume_header(&mut self, module: mr::ModuleHeader) -> Action;
    /// Consume the given instruction.
    fn consume_instruction(&mut self, inst: mr::Instruction) -> Action;
}

/// Parses the given `binary` and consumes the module using the given
/// `consumer`.
pub fn parse(binary: Vec<u8>, consumer: &mut Consumer) -> Result<()> {
    Parser::new(binary, consumer).parse()
}

/// The SPIR-V binary parser.
///
/// Takes in a vector of bytes and a consumer, this parser will invoke the
/// consume methods on the consumer for the module header and each
/// instruction parsed.
///
/// Different from the [`Decoder`](struct.Decoder.html),
/// this parser is high-level; it has knowlege of the SPIR-V grammar.
/// It will parse instructions according to SPIR-V grammar.
pub struct Parser<'a> {
    decoder: decoder::Decoder,
    consumer: &'a mut Consumer,
    /// The index of the current instructions
    ///
    /// Starting from 1, 0 means invalid
    inst_index: usize,
}

/// Tries to decode `$e` and returns the error if errored out.
macro_rules! try_decode {
    ($e: expr) => (match $e {
        Ok(val) => val,
        Err(err) => return Err(State::OperandError(err))
    });
}

impl<'a> Parser<'a> {
    /// Creates a new parser to parse the given `binary` and send the module
    /// header and instructions to the given `consumer`.
    pub fn new(binary: Vec<u8>, consumer: &'a mut Consumer) -> Parser<'a> {
        Parser {
            decoder: decoder::Decoder::new(binary),
            consumer: consumer,
            inst_index: 0,
        }
    }

    /// Does the parsing.
    pub fn parse(mut self) -> Result<()> {
        match self.consumer.initialize() {
            Action::Continue => (),
            Action::Stop => return Err(State::ConsumerStopRequested),
            Action::Error(err) => return Err(State::ConsumerError(err)),
        }
        let header = try!(self.parse_header());
        match self.consumer.consume_header(header) {
            Action::Continue => (),
            Action::Stop => return Err(State::ConsumerStopRequested),
            Action::Error(err) => return Err(State::ConsumerError(err)),
        }

        loop {
            let result = self.parse_inst();
            match result {
                Ok(inst) => {
                    match self.consumer.consume_instruction(inst) {
                        Action::Continue => (),
                        Action::Stop => {
                            return Err(State::ConsumerStopRequested)
                        }
                        Action::Error(err) => {
                            return Err(State::ConsumerError(err))
                        }
                    }
                }
                Err(State::Complete) => break,
                Err(error) => return Err(error),
            };
        }
        match self.consumer.finalize() {
            Action::Continue => (),
            Action::Stop => return Err(State::ConsumerStopRequested),
            Action::Error(err) => return Err(State::ConsumerError(err)),
        }
        Ok(())
    }

    fn split_into_word_count_and_opcode(word: spirv::Word) -> (u16, u16) {
        ((word >> 16) as u16, (word & 0xffff) as u16)
    }

    fn parse_header(&mut self) -> Result<mr::ModuleHeader> {
        match self.decoder.words(HEADER_NUM_WORDS) {
            Ok(words) => {
                if words[0] != MAGIC_NUMBER {
                    if words[0] == MAGIC_NUMBER.swap_bytes() {
                        return Err(State::EndiannessUnsupported);
                    } else {
                        return Err(State::HeaderIncorrect);
                    }
                }
                Ok(mr::ModuleHeader::new(words[0],
                                         words[1],
                                         words[2],
                                         words[3],
                                         words[4]))
            }
            Err(err) => Err(State::HeaderIncomplete(err)),
        }
    }

    fn parse_inst(&mut self) -> Result<mr::Instruction> {
        self.inst_index += 1;
        if let Ok(word) = self.decoder.word() {
            let (wc, opcode) = Parser::split_into_word_count_and_opcode(word);
            if wc == 0 {
                return Err(State::WordCountZero(self.decoder.offset() - 1,
                                                self.inst_index));
            }
            if let Some(grammar) = GInstTable::lookup_opcode(opcode) {
                self.decoder.set_limit((wc - 1) as usize);
                let result = self.parse_operands(grammar);
                if !self.decoder.limit_reached() {
                    return Err(State::OperandExceeded(self.decoder.offset(),
                                                      self.inst_index));
                }
                self.decoder.clear_limit();
                result
            } else {
                Err(State::OpcodeUnknown(self.decoder.offset() - 1,
                                         self.inst_index,
                                         opcode))
            }
        } else {
            Err(State::Complete)
        }
    }

    fn parse_operands(&mut self, grammar: GInstRef) -> Result<mr::Instruction> {
        let mut rtype = None;
        let mut rid = None;
        let mut coperands = vec![]; // concrete operands

        let mut loperand_index: usize = 0; // logical operand index
        while loperand_index < grammar.operands.len() {
            let loperand = &grammar.operands[loperand_index];
            let has_more_coperands = !self.decoder.limit_reached();
            if has_more_coperands {
                match loperand.kind {
                    GOpKind::IdResultType => {
                        rtype = Some(try_decode!(self.decoder.id()))
                    }
                    GOpKind::IdResult => {
                        rid = Some(try_decode!(self.decoder.id()))
                    }
                    _ => coperands.append(
                        &mut try!(self.parse_operand(loperand.kind))),
                }
                match loperand.quantifier {
                    GOpCount::One | GOpCount::ZeroOrOne => loperand_index += 1,
                    GOpCount::ZeroOrMore => continue,
                }
            } else {
                // We still have logical operands to match but no no more words.
                match loperand.quantifier {
                    GOpCount::One => {
                        return Err(State::OperandExpected(self.decoder
                                                              .offset() -
                                                          1,
                                                          self.inst_index))
                    }
                    GOpCount::ZeroOrOne | GOpCount::ZeroOrMore => break,
                }
            }
        }
        Ok(mr::Instruction::new(grammar, rtype, rid, coperands))
    }
}

include!("parse_operand.rs");