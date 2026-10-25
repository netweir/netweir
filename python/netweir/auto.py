"""Items read from a page without selectors.

Each class here is an ``Item`` with a fixed set of fields, named as in the
common item schema (in snake_case), that Netweir fills from the page's
structured data: JSON-LD, microdata and meta tags::

    product = page.extract(netweir.auto.Product)
    product["name"], product["price"], product["currency"]

Every item ends with ``metadata``: ``probability`` (how sure Netweir is
the page holds one of these, 0 to 1), ``date_downloaded``, ``sources``
(where each field came from) and ``validation_messages``.

When one field comes out wrong for a site, subclass and pin that field
with ``netweir.css`` or ``netweir.xpath``; the rest stay automatic::

    class Shop(netweir.auto.Product):
        price = netweir.css("span.sale", track="price")

A pinned field is read as that field (a price element gives ``"9.50"``)
unless it has ``into=``. Fields the type doesn't have are kept after its
own. Subclasses work in rules like any Item, without Python per page.
"""

from __future__ import annotations

from netweir._rules import Item


class Product(Item):
    """A product page's product: name, price, currency, stock, codes,
    brand, images, rating, breadcrumbs, variants and more."""

    _schema = "product"


class ProductList(Item):
    """A category or search page's products, breadcrumbs and next page."""

    _schema = "product-list"


class ProductNavigation(Item):
    """Where a category page leads: subcategories, products, next page."""

    _schema = "product-navigation"


class JobPosting(Item):
    """A job advert: title, organisation, location, salary, dates and
    description."""

    _schema = "job-posting"


class Article(Item):
    """An article in the common schema's shape: headline, authors, dates,
    images, body. (``page.article()`` gives ``netweir.Article``, a
    different and simpler shape.)"""

    _schema = "article"


__all__ = ["Article", "JobPosting", "Product", "ProductList", "ProductNavigation"]
