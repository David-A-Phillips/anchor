//! Realistic source fixtures shared by the parser tests.

pub const RUST: &str = r#"//! Ledger crate.
//! Second line.

use std::collections::{HashMap, HashSet};
use crate::billing::Invoice as Inv;

/// A ledger of charges.
#[derive(Debug)]
pub struct Ledger<T> {
    entries: Vec<T>,
}

impl<T> Ledger<T> {
    /// Creates one.
    pub fn new() -> Self {
        Self::helper();
        Ledger { entries: Vec::new() }
    }

    pub(crate) fn charge(&mut self, amount: Money) {
        self.audit.record(amount);
        println!("x");
    }

    fn helper() {}
}

impl Display for Ledger<u8> {
    fn fmt(&self) {}
}

pub trait Audit {
    fn record(&self);
}

mod inner {
    pub fn f() { crate::util::g(); }
}
"#;

pub const PYTHON: &str = r#""""Billing helpers.

More text.
"""
import os.path
from . import sibling
from .ledger import Ledger, charge as do_charge
from ..pkg.mod import *

MAX_RETRIES = 3

@decorator(arg)
class Account(Base, mixins.Audit):
    """An account."""

    def __init__(self, ledger: Ledger) -> None:
        self.ledger = ledger

    async def charge(self, amount: int) -> Optional[Receipt]:
        """Charge it.

        Details.
        """
        self.ledger.record(amount)
        return helper(amount)

    def _private(self):
        pass


def helper(x):
    def inner():
        pass
    return inner()
"#;

pub const TS: &str = r#"/** Module comment. */

import { Ledger, type Money } from './ledger';
import axios from 'axios';
import * as fs from "node:fs";
const lodash = require('lodash/fp');

/**
 * A service.
 */
export class Service<T> extends Base implements Runnable {
  private count: number = 0;
  #secret = 1;

  constructor(private ledger: Ledger) { super(); }

  /** Runs. */
  async run(input: T): Promise<Money> {
    const l = new Ledger();
    this.ledger.charge(input);
    return helper(input);
  }

  protected tick = () => { audit(); };
}

export interface Runnable { run(input: number): void; }
export type Id = string;
export enum Mode { A, B }
export const helper = (x: number): number => x + 1;
const LIMIT = 10;
function local() {}
export default function main() { local(); }
namespace Util { export function f() {} }
"#;

pub const JS: &str = r#"import { a } from "../lib/a.js";

class Counter {
  inc() { return util.bump(this.n); }
}

const make = function () { return new Counter(); };
"#;

pub const TSX: &str = r#"import React from 'react';
import { Button } from './Button';

export function App({ name }: Props) {
  return <Button label={name} />;
}
"#;

pub const GO: &str = r#"// Package ledger does ledger things.
package ledger

import (
	"fmt"
	str "strings"
	"github.com/acme/money"
)

// MaxEntries is a limit.
const MaxEntries = 10

var count int

// Ledger holds entries.
type Ledger struct {
	entries []money.Amount
}

// Store stores.
type Store interface {
	Save(l *Ledger) error
}

type ID = string

// New creates a ledger.
func New() *Ledger {
	fmt.Println("x")
	return &Ledger{}
}

// Charge charges.
func (l *Ledger) Charge(a money.Amount) error {
	l.audit(a)
	return helper(a)
}

func helper(a money.Amount) error { return nil }
"#;

pub const JAVA: &str = r#"package com.acme.payments;

import java.util.List;
import com.acme.money.Money;
import static com.acme.Util.helper;
import java.io.*;

/** A ledger. */
public class Ledger<T> extends Base implements Auditable {
    public static final int MAX = 10;
    private List<Money> entries;

    public Ledger() {}

    /** Charges. */
    public void charge(Money amount) {
        this.audit(amount);
        Util.log(amount);
        Ledger<String> other = new Ledger<>();
    }

    void packagePrivate() {}

    protected abstract int size();

    enum Mode { A, B }
}

interface Auditable {
    void audit(Money m);
}
"#;

pub const C: &str = r#"/* Ledger implementation. */
#include <stdio.h>
#include "ledger.h"

#define MAX_ENTRIES 10
#define SQUARE(x) ((x) * (x))

typedef struct Entry {
    int amount;
} Entry;

typedef struct { int a; } Pair;

typedef int (*callback_t)(int);

struct Ledger {
    Entry *entries;
};

enum Mode { A, B };

static int counter = 0;
int total;
int helper(int x);

/** Charges the ledger. */
int ledger_charge(struct Ledger *l, int amount) {
    printf("x");
    l->audit(amount);
    return helper(amount);
}

static void hidden(void) {}
"#;

pub const ALL: [(super::Language, &str); 8] = [
    (super::Language::Rust, RUST),
    (super::Language::Python, PYTHON),
    (super::Language::TypeScript, TS),
    (super::Language::JavaScript, JS),
    (super::Language::Tsx, TSX),
    (super::Language::Go, GO),
    (super::Language::Java, JAVA),
    (super::Language::C, C),
];
